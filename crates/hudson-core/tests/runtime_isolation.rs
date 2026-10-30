use hudson_core::adapters::runtime_bridge::*;
use serde_json::json;

fn settings(transport: serde_json::Value) -> BridgeConfig {
    serde_json::from_value(json!({"id":"isolated","version":1,"transport":transport})).unwrap()
}

#[test]
fn isolated_stdio_requires_explicit_disjoint_mounts_and_bounded_tmpfs() {
    let base = json!({"type":"isolated_stdio","executable":"/usr/bin/python3.12","cwd":"/srv/work",
        "isolation":{"bubblewrap":"/usr/bin/bwrap","read_only":["/usr"],"network":"deny"}});
    if cfg!(target_os = "linux") {
        assert!(settings(base.clone()).validate().is_ok());
        for mounts in [
            json!(["/"]),
            json!(["/usr", "/usr/lib"]),
            json!(["/srv"]),
            json!(["/dev"]),
        ] {
            let mut value = base.clone();
            value["isolation"]["read_only"] = mounts;
            assert!(settings(value).validate().is_err());
        }
        let mut value = base.clone();
        value["isolation"]["tmp_bytes"] = json!(0);
        assert!(settings(value).validate().is_err());
    } else {
        assert!(settings(base.clone()).validate().is_err());
    }
    let mut implicit_network = base;
    implicit_network["isolation"]
        .as_object_mut()
        .unwrap()
        .remove("network");
    assert!(serde_json::from_value::<BridgeConfig>(
        json!({"id":"isolated","version":1,"transport":implicit_network})
    )
    .is_err());
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use hudson_core::adapters::models::ModelExecutor;
    use hudson_harness::{ModelRequest, ModelResponse};
    use std::{io::Write, net::TcpListener, path::Path, time::Duration};

    fn launcher() -> String {
        std::env::var("HUDSON_TEST_BWRAP")
            .expect("set HUDSON_TEST_BWRAP to the real bubblewrap executable")
    }
    fn request() -> ModelRequest {
        ModelRequest {
            instructions: "fixture".into(),
            model: "fixture".into(),
            messages: vec![],
            tools: vec![],
        }
    }
    fn transport(cwd: &Path, read_only: &Path, code: String, network: &str) -> serde_json::Value {
        json!({"type":"isolated_stdio","executable":"/usr/bin/python3.12","args":["-I","-c",code],"cwd":cwd,
            "isolation":{"bubblewrap":launcher(),"read_only":["/usr","/lib","/lib64",read_only],"network":network}})
    }

    #[test]
    #[ignore = "requires real Linux bubblewrap; CI runs with --include-ignored"]
    fn real_isolated_stdio_enforces_filesystem_and_explicit_network_grants() {
        let workspace = tempfile::tempdir_in("/var/tmp").unwrap();
        let readonly = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut hidden = tempfile::NamedTempFile::new_in("/var/tmp").unwrap();
        hidden.write_all(b"host-only").unwrap();
        let canary = readonly.path().join("canary");
        std::fs::write(&canary, b"unchanged").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        for network in ["deny", "host"] {
            let code = format!(
                r#"
import json,os,socket,sys
json.load(sys.stdin)
assert not os.path.exists({hidden})
assert open({canary}).read() == 'unchanged'
try:
    open({canary}, 'w').close()
except OSError:
    pass
else:
    raise AssertionError('read-only grant became writable')
open('receipt', 'w').write('owned workspace')
assert os.getpid() == 2
s = socket.socket(); s.settimeout(2)
assert (s.connect_ex(('127.0.0.1', {port})) == 0) == {allowed}
s.close()
print(json.dumps({{'protocol_version':1,'runtime_id':'isolated','runtime_version':1,'response':{{'type':'final','output':42}}}}))
"#,
                hidden = json!(hidden.path()),
                canary = json!(canary),
                allowed = if network == "host" { "True" } else { "False" }
            );
            let config = settings(transport(workspace.path(), readonly.path(), code, network));
            assert_eq!(config.descriptor().transport, "isolated_stdio_bridge");
            let mut model = BridgeModel::new(config, 32).unwrap();
            assert_eq!(
                model.call(&request()).unwrap(),
                ModelResponse::Final { output: json!(42) }
            );
            assert_eq!(std::fs::read(&canary).unwrap(), b"unchanged");
            assert!(workspace.path().join("receipt").is_file());
        }
        // Even a launcher that exits before setup must never run the target on
        // the host. The target's marker would expose such a fallback.
        let marker = workspace.path().join("fallback");
        let code = "open('fallback', 'w').write('unsafe')".to_owned();
        let mut value = transport(workspace.path(), readonly.path(), code, "deny");
        value["isolation"]["bubblewrap"] = json!("/usr/bin/false");
        let mut model = BridgeModel::new(settings(value), 32).unwrap();
        assert!(model.call(&request()).is_err());
        assert!(!marker.exists());
    }

    #[test]
    #[ignore = "requires real Linux bubblewrap; CI runs with --include-ignored"]
    fn real_isolated_stdio_cancellation_kills_descendants_that_change_process_group() {
        let workspace = tempfile::tempdir_in("/var/tmp").unwrap();
        let readonly = tempfile::tempdir_in("/var/tmp").unwrap();
        let child = "import time; f=open('heartbeat','ab',buffering=0)\nwhile True: f.write(b'.'); time.sleep(.05)";
        let code = format!("import json,sys,subprocess,time; json.load(sys.stdin); subprocess.Popen([sys.executable,'-I','-c',{}], start_new_session=True); time.sleep(30)", json!(child));
        let mut model = BridgeModel::new(
            settings(transport(workspace.path(), readonly.path(), code, "deny")),
            32,
        )
        .unwrap();
        let heartbeat = workspace.path().join("heartbeat");
        let start = std::time::Instant::now();
        let result = model.call_controlled(&request(), &|| {
            heartbeat.exists() || start.elapsed() > Duration::from_secs(10)
        });
        assert!(result.is_err());
        assert!(
            heartbeat.exists(),
            "the isolated descendant must actually start"
        );
        std::thread::sleep(Duration::from_millis(250));
        let stopped = std::fs::metadata(&heartbeat).unwrap().len();
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(std::fs::metadata(&heartbeat).unwrap().len(), stopped);
    }
}
