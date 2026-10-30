use hudson_core::{
    adapters::{
        models::{ModelExecutor, ModelObserver},
        runtime_bridge::*,
        tools::ExecutionError,
    },
    configured::Configuration,
    models::*,
    storage::Store,
};
use hudson_harness::{ModelRequest, ModelResponse};
use serde_json::{json, Value};
#[cfg(unix)]
use std::time::Instant;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    time::Duration,
};

fn request() -> ModelRequest {
    ModelRequest {
        instructions: "help".into(),
        model: "fixture".into(),
        messages: vec![],
        tools: vec![],
    }
}
fn reply(response: Value) -> Value {
    json!({"protocol_version":1,"runtime_id":"fixture","runtime_version":1,"response":response,
        "usage":{"input_tokens":10,"output_tokens":2}})
}
fn config(endpoint: String) -> BridgeConfig {
    serde_json::from_value(
        json!({"id":"fixture","version":1,"transport":{"type":"http","endpoint":endpoint}}),
    )
    .unwrap()
}

#[test]
fn explicit_reasoning_requires_an_exact_receipt_and_preserves_usage_and_output_cap() {
    use hudson_core::adapters::reasoning::Effort;
    for received in [None, Some(Effort::High), Some(Effort::Low)] {
        let mut response = reply(json!({"type":"final","output":42}));
        if let Some(effort) = received {
            response["reasoning_effort"] = json!(effort);
        }
        let (endpoint, requests, peer) = serve(vec![response]);
        let mut settings = config(endpoint);
        assert!(serde_json::to_value(&settings)
            .unwrap()
            .get("reasoning_effort")
            .is_none());
        settings.reasoning_effort = Some(Effort::Low);
        assert!(settings.descriptor().capabilities.reasoning_effort);
        let mut model = BridgeModel::new(settings, 32).unwrap();
        assert_eq!(
            model.call(&request()).is_ok(),
            received == Some(Effort::Low)
        );
        assert_eq!(model.take_usage().unwrap().input_tokens, 10);
        assert!(model.take_usage().is_none());
        peer.join().unwrap();
        let sent = requests.lock().unwrap();
        assert_eq!(sent[0]["reasoning_effort"], "low");
        assert_eq!(sent[0]["max_output_tokens"], 32);
    }
}
fn write_config(value: &Value) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    serde_json::to_writer(&mut file, value).unwrap();
    file
}
fn read_request(stream: &mut std::net::TcpStream) -> Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
        assert!(header.len() < 16384);
    }
    let text = String::from_utf8(header).unwrap();
    let len = text
        .lines()
        .find_map(|line| {
            line.to_lowercase()
                .strip_prefix("content-length:")
                .map(|s| s.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    assert!(len <= 4 * 1024 * 1024);
    let mut body = vec![0; len];
    stream.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}
fn serve(replies: Vec<Value>) -> (String, Arc<Mutex<Vec<Value>>>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let thread = std::thread::spawn(move || {
        for response in replies {
            let (mut stream, _) = listener.accept().unwrap();
            captured.lock().unwrap().push(read_request(&mut stream));
            let body = response.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    });
    (endpoint, requests, thread)
}

#[derive(Default)]
struct CollectedText {
    text: String,
}
impl ModelObserver for CollectedText {
    fn cancelled(&self) -> bool {
        false
    }
    fn text_delta(&mut self, text: &str) -> Result<(), ExecutionError> {
        self.text.push_str(text);
        Ok(())
    }
}

#[test]
fn native_sessions_require_admission_and_reject_stale_checkpoint_receipts() {
    use hudson_core::adapters::models::{
        ModelCallContext, RuntimeCallContext, RuntimeCheckpoint, RuntimeHistoryMode,
    };
    let mut settings = config("http://127.0.0.1:1/model".into());
    settings.native_sessions = true;
    let mut model = BridgeModel::new(settings, 16).unwrap();
    assert!(matches!(
        model.call(&request()),
        Err(ExecutionError::Failed(_))
    ));
    struct Observer(RuntimeCallContext);
    impl ModelObserver for Observer {
        fn runtime_context(
            &self,
            _: &ModelRequest,
        ) -> Result<Option<RuntimeCallContext>, ExecutionError> {
            Ok(Some(self.0.clone()))
        }
        fn cancelled(&self) -> bool {
            false
        }
        fn text_delta(&mut self, _: &str) -> Result<(), ExecutionError> {
            Ok(())
        }
    }
    let call = ModelCallContext {
        run_id: uuid::Uuid::new_v4(),
        operation_id: uuid::Uuid::new_v4(),
        attempt_id: uuid::Uuid::new_v4(),
    };
    let context = RuntimeCallContext {
        call,
        actor: Actor {
            workspace_id: "fixture".into(),
            id: "owner".into(),
        },
        previous: None,
        history_mode: RuntimeHistoryMode::Replace,
        request_fingerprint: "expected".into(),
    };
    let mut response = reply(json!({"type":"final","output":42}));
    response["checkpoint"] = serde_json::to_value(RuntimeCheckpoint {
        run_id: call.run_id,
        operation_id: call.operation_id,
        attempt_id: call.attempt_id,
        request_fingerprint: "stale".into(),
        input_messages: 0,
    })
    .unwrap();
    let (endpoint, requests, peer) = serve(vec![response]);
    let mut settings = config(endpoint);
    settings.native_sessions = true;
    let mut model = BridgeModel::new(settings, 16).unwrap();
    assert!(matches!(
        model.call_observed(&request(), &mut Observer(context)),
        Err(ExecutionError::Failed(_))
    ));
    assert_eq!(model.take_usage().unwrap().input_tokens, 10);
    assert!(model.take_usage().is_none());
    peer.join().unwrap();
    assert_eq!(
        requests.lock().unwrap()[0]["context"]["call"]["attempt_id"],
        json!(call.attempt_id)
    );
}
fn stream_peer(frames: Vec<Value>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let peer = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_nodelay(true).unwrap();
        let request = read_request(&mut socket);
        assert_eq!(request["stream"], true);
        let data = frames
            .iter()
            .map(|v| format!("{v}\r\n"))
            .collect::<String>();
        if write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",data.len()).is_err(){return;}
        // Fragment records across UTF-8 and JSON boundaries.
        for chunk in data.as_bytes().chunks(7) {
            if socket.write_all(chunk).is_err() {
                return;
            }
        }
    });
    (endpoint, peer)
}
#[test]
fn streaming_bridge_delivers_fragments_and_preserves_unicode_and_final_usage() {
    let (endpoint, peer) = stream_peer(vec![
        json!({"type":"start","protocol_version":1,"runtime_id":"fixture","runtime_version":1}),
        json!({"type":"text_delta","text":"hé\u{2028}🌍"}),
        json!({"type":"text_delta","text":"42"}),
        json!({"type":"response","result":reply(json!({"type":"final","output":42}))}),
    ]);
    let mut settings = config(endpoint);
    settings.stream = true;
    assert!(settings.descriptor().capabilities.streaming);
    let mut model = BridgeModel::new(settings, 32).unwrap();
    let mut observer = CollectedText::default();
    assert_eq!(
        model.call_observed(&request(), &mut observer).unwrap(),
        ModelResponse::Final { output: json!(42) }
    );
    assert_eq!(observer.text, "hé\u{2028}🌍42");
    assert_eq!(model.take_usage().unwrap().input_tokens, 10);
    peer.join().unwrap();
}
#[test]
fn streaming_bridge_rejects_wrong_identity_truncation_and_frames_after_final() {
    let start =
        json!({"type":"start","protocol_version":1,"runtime_id":"fixture","runtime_version":1});
    let final_frame =
        json!({"type":"response","result":reply(json!({"type":"final","output":42}))});
    for frames in [
        vec![
            json!({"type":"start","protocol_version":1,"runtime_id":"other","runtime_version":1}),
            final_frame.clone(),
        ],
        vec![start.clone(), json!({"type":"text_delta","text":"partial"})],
        vec![
            start,
            final_frame,
            json!({"type":"text_delta","text":"late"}),
        ],
    ] {
        let (endpoint, peer) = stream_peer(frames);
        let mut settings = config(endpoint);
        settings.stream = true;
        let mut model = BridgeModel::new(settings, 32).unwrap();
        let mut observer = CollectedText::default();
        assert!(model.call_observed(&request(), &mut observer).is_err());
        assert!(model.take_usage().is_none());
        peer.join().unwrap();
    }
}

#[test]
fn validates_transport_and_limits_without_io() {
    let valid = config("http://127.0.0.1:1/bridge".into());
    valid.validate().unwrap();
    for endpoint in [
        "http://remote.invalid/",
        "https://user:secret@example.com/",
        "https://example.com/?secret=x",
        "https://example.com/#x",
    ] {
        assert!(config(endpoint.into()).validate().is_err());
    }
    for (field, value) in [
        ("version", json!(0)),
        ("timeout_seconds", json!(0)),
        ("timeout_seconds", json!(301)),
        ("max_request_bytes", json!(0)),
        ("max_response_bytes", json!(4194305)),
        ("id", json!("bad.id")),
    ] {
        let mut value_config = serde_json::to_value(&valid).unwrap();
        value_config[field] = value;
        assert!(serde_json::from_value::<BridgeConfig>(value_config)
            .unwrap()
            .validate()
            .is_err());
    }
    let secret_free = serde_json::to_value(valid.descriptor())
        .unwrap()
        .to_string();
    assert!(!secret_free.contains("127.0.0.1"));
    assert!(!secret_free.contains("endpoint"));
}

#[tokio::test]
async fn async_host_can_release_a_bridge_without_blocking_runtime_shutdown() {
    let model = tokio::task::spawn_blocking(|| {
        BridgeModel::new(config("http://127.0.0.1:1/bridge".into()), 16).unwrap()
    })
    .await
    .unwrap();
    drop(model);
}

fn approval_cycle(store: Store) {
    let call = json!({"type":"tool_calls","calls":[{"call_id":"lookup-1","name":"lookup","arguments":{"id":42}}]});
    let (endpoint, requests, peer) = serve(vec![
        reply(call),
        reply(json!({"type":"final","output":{"answer":42}})),
        reply(json!({"type":"final","output":{"answer":43}})),
    ]);
    let (tool_endpoint, tool_calls, tool_peer) = serve(vec![json!({"answer":42})]);
    let source = write_config(
        &json!({"name":"bridge-agent","instructions":"Return the tool's answer","model":"fixture",
        "runtime":config(endpoint),"shared_model_budget":{"group":format!("bridge-{}",uuid::Uuid::new_v4()),"limit":3},
        "http_tools":[{"name":"lookup","description":"lookup","endpoint":tool_endpoint,"input_schema":{"type":"object"},
            "effect":"read","require_approval":true}],"output_schema":{"type":"object","required":["answer"]}}),
    );
    let actor = Actor {
        workspace_id: "bridge-tests".into(),
        id: "owner".into(),
    };
    let configuration = Configuration::load(source.path()).unwrap();
    assert_eq!(configuration.execution_catalog().len(), 1);
    let mut tree = configuration
        .clone()
        .build_tree(store.clone(), &actor)
        .unwrap();
    let id = tree
        .runtime
        .submit(
            &actor,
            tree.reference.clone(),
            json!("lookup 42"),
            Some("submission".into()),
        )
        .unwrap();
    for _ in 0..8 {
        if tree.tick(&actor, id).unwrap().status == RunStatus::Waiting {
            break;
        }
    }
    let operation = store
        .operations(&actor, id)
        .unwrap()
        .into_iter()
        .find(|op| op.status == OperationStatus::WaitingApproval)
        .expect("approval wait");
    assert_eq!(tool_calls.lock().unwrap().len(), 0);
    assert_eq!(requests.lock().unwrap().len(), 1);
    drop(tree);
    let mut restored = configuration.build_tree(store.clone(), &actor).unwrap();
    restored
        .runtime
        .approve(&actor, operation.meta.id, true, now(), now() + 300_000)
        .unwrap();
    for _ in 0..12 {
        if restored.tick(&actor, id).unwrap().status.terminal() {
            break;
        }
    }
    let run = store.inspect(&actor, id).unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.result, Some(json!({"answer":42})));
    assert_eq!(run.usage.model_calls, 2);
    assert_eq!(run.usage.reported_model_calls, 2);
    assert_eq!(run.usage.reported_input_tokens, 20);
    let next = restored
        .runtime
        .submit_turn(
            &actor,
            restored.reference.clone(),
            json!("continue"),
            Some("next-turn".into()),
            id,
        )
        .unwrap();
    for _ in 0..8 {
        if restored.tick(&actor, next).unwrap().status.terminal() {
            break;
        }
    }
    assert_eq!(
        store.inspect(&actor, next).unwrap().result,
        Some(json!({"answer":43}))
    );
    assert_eq!(
        store.inspect(&actor, id).unwrap().result,
        Some(json!({"answer":42}))
    );
    peer.join().unwrap();
    tool_peer.join().unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0]["protocol_version"], 1);
    assert_eq!(requests[0]["max_output_tokens"], 1024);
    assert!(requests[1]["request"]["messages"]
        .to_string()
        .contains("lookup-1"));
    assert!(requests[2]["request"]["messages"]
        .to_string()
        .contains("lookup-1"));
    assert!(requests[2]["request"]["messages"]
        .to_string()
        .contains("continue"));
    assert_eq!(tool_calls.lock().unwrap().len(), 1);
}
#[test]
fn tool_approval_usage_and_history_survive_adapter_reconstruction() {
    approval_cycle(Store::default());
}
#[test]
#[ignore = "requires dedicated PostgreSQL database on /tmp"]
fn postgres_bridge_approval_cycle() {
    let database = std::env::var("HUDSON_TEST_DATABASE").expect("dedicated test database");
    approval_cycle(
        Store::postgres_local(
            "/tmp",
            &database,
            &format!("bridge-{}", uuid::Uuid::new_v4()),
        )
        .unwrap(),
    );
}

#[test]
fn rejects_wrong_identity_and_unadvertised_tools_without_stale_usage() {
    let mut wrong = reply(json!({"type":"final","output":42}));
    wrong["runtime_version"] = json!(2);
    let (endpoint, _, peer) = serve(vec![
        reply(json!({"type":"final","output":42})),
        wrong,
        reply(
            json!({"type":"tool_calls","calls":[{"call_id":"bad","name":"shell","arguments":{}}]}),
        ),
    ]);
    let mut model = BridgeModel::new(config(endpoint), 16).unwrap();
    model.call(&request()).unwrap();
    assert_eq!(model.take_usage().unwrap().input_tokens, 10);
    assert!(model.call(&request()).is_err());
    assert!(model.take_usage().is_none());
    assert!(model.call(&request()).is_err());
    // A valid provider usage report still belongs to the rejected call.
    assert_eq!(model.take_usage().unwrap().input_tokens, 10);
    assert!(model.take_usage().is_none());
    peer.join().unwrap();
}

#[test]
fn output_limit_and_pre_dispatch_request_limit_fail_closed() {
    let (endpoint, requests, peer) = serve(vec![reply(
        json!({"type":"final","output":"x".repeat(1024)}),
    )]);
    let mut settings = config(endpoint);
    settings.max_response_bytes = 256;
    let mut model = BridgeModel::new(settings.clone(), 16).unwrap();
    assert!(model.call(&request()).is_err());
    peer.join().unwrap();
    settings.max_request_bytes = 1;
    let mut model = BridgeModel::new(settings, 16).unwrap();
    assert!(model.call(&request()).is_err());
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn pins_adapter_settings_and_admission_never_resolves_credentials() {
    let mut settings = serde_json::to_value(config("https://bridge.invalid/".into())).unwrap();
    settings["transport"]["bearer_env"] = json!("HUDSON_BRIDGE_ABSENT_TEST_CREDENTIAL");
    let mut definition =
        json!({"name":"pinned","instructions":"help","model":"fixture","runtime":settings});
    let actor = Actor {
        workspace_id: "bridge-tests".into(),
        id: "owner".into(),
    };
    let store = Store::default();
    let source = write_config(&definition);
    let configuration = Configuration::load(source.path()).unwrap();
    configuration
        .build_admission_tree(store.clone(), &actor)
        .unwrap();
    definition["runtime"]["timeout_seconds"] = json!(60);
    let changed = write_config(&definition);
    assert!(Configuration::load(changed.path())
        .unwrap()
        .build_admission_tree(store, &actor)
        .is_err());
    for key in ["provider", "endpoint", "api_key_env", "reasoning"] {
        let mut conflicting = definition.clone();
        conflicting[key] = match key {
            "reasoning" => json!({"type":"effort","effort":"low"}),
            _ => json!("invalid"),
        };
        assert!(Configuration::load(write_config(&conflicting).path()).is_err());
    }
}

#[cfg(unix)]
fn stdio(script: String) -> BridgeConfig {
    serde_json::from_value(
        json!({"id":"fixture","version":1,"transport":{"type":"stdio",
        "executable":"/usr/bin/python3","args":["-c",script],"cwd":"/tmp"}}),
    )
    .unwrap()
}
#[cfg(unix)]
#[test]
fn stdio_roundtrip_and_environment_are_bounded_and_explicit() {
    let output = reply(json!({"type":"final","output":42})).to_string();
    let script = format!("import json,sys,os\nr=json.load(sys.stdin)\nassert r['runtime_id']=='fixture'\nassert 'HOME' not in os.environ\nprint({output:?})");
    let mut model = BridgeModel::new(stdio(script), 16).unwrap();
    assert_eq!(
        model.call(&request()).unwrap(),
        ModelResponse::Final { output: json!(42) }
    );
    assert_eq!(model.take_usage().unwrap().output_tokens, 2);
}
#[cfg(unix)]
#[test]
fn interruption_kills_process_group_and_redacts_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let script = format!("import subprocess,time\np=subprocess.Popen(['/bin/sleep','30'])\nopen({:?},'w').write(str(p.pid))\ntime.sleep(30)",pid_file.to_str().unwrap());
    let mut model = BridgeModel::new(stdio(script), 16).unwrap();
    let start = Instant::now();
    let result =
        model.call_controlled(&request(), &|| start.elapsed() > Duration::from_millis(500));
    assert!(matches!(result, Err(ExecutionError::Unknown(_))));
    assert!(start.elapsed() < Duration::from_secs(3));
    let pid = std::fs::read_to_string(pid_file).unwrap();
    let terminated = Instant::now();
    loop {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        // Signal delivery is asynchronous. Orphan zombies can await init
        // reaping; they cannot execute or retain pipes.
        if status.is_empty()
            || status
                .lines()
                .any(|s| s.starts_with("State:") && s.contains("Z (zombie)"))
        {
            break;
        }
        assert!(
            terminated.elapsed() < Duration::from_secs(2),
            "descendant did not terminate: {status}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut model = BridgeModel::new(
        stdio("import sys\nsys.stderr.write('secret-diagnostic')\nsys.exit(7)".into()),
        16,
    )
    .unwrap();
    let error = format!("{:?}", model.call(&request()).unwrap_err());
    assert!(!error.contains("secret-diagnostic"));
}
#[cfg(unix)]
#[test]
fn deadline_and_stderr_flood_do_not_deadlock_or_retry() {
    let mut settings = stdio("import time\ntime.sleep(30)".into());
    settings.timeout_seconds = 1;
    let mut model = BridgeModel::new(settings, 16).unwrap();
    let start = Instant::now();
    assert!(matches!(
        model.call(&request()),
        Err(ExecutionError::Unknown(_))
    ));
    assert!(start.elapsed() < Duration::from_secs(3));
    let mut model = BridgeModel::new(
        stdio(
            "import sys,time\nsys.stderr.write('x'*70000)\nsys.stderr.flush()\ntime.sleep(30)"
                .into(),
        ),
        16,
    )
    .unwrap();
    let start = Instant::now();
    assert!(model.call(&request()).is_err());
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[cfg(unix)]
#[test]
fn runtime_cancellation_reaches_a_budgeted_external_process() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started");
    let script = format!(
        "import time\nopen({:?},'w').write('started')\ntime.sleep(30)",
        marker.to_str().unwrap()
    );
    let source = write_config(
        &json!({"name":"interrupt","instructions":"help","model":"fixture","runtime":stdio(script),
        "shared_model_budget":{"group":"cancel-group","limit":1}}),
    );
    let configuration = Configuration::load(source.path()).unwrap();
    let store = Store::default();
    let actor = Actor {
        workspace_id: "bridge-tests".into(),
        id: "owner".into(),
    };
    let mut driver = configuration
        .clone()
        .build_tree(store.clone(), &actor)
        .unwrap();
    let id = driver
        .runtime
        .submit(&actor, driver.reference.clone(), json!("wait"), None)
        .unwrap();
    driver.tick(&actor, id).unwrap();
    let owner = actor.clone();
    let worker = std::thread::spawn(move || driver.tick(&owner, id));
    let start = Instant::now();
    while !marker.exists() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    let control = configuration
        .build_admission_tree(store.clone(), &actor)
        .unwrap();
    control.runtime.cancel(&actor, id).unwrap();
    let view = worker.join().unwrap().unwrap();
    assert!(matches!(
        view.status,
        RunStatus::Cancelled | RunStatus::Cancelling
    ));
    assert!(view.result.is_none());
    assert_eq!(view.usage.model_calls, 1);
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(store.operations(&actor, id).unwrap().len(), 1);
}

#[cfg(windows)]
#[test]
#[ignore = "invoked as the isolated child of the Windows containment regression"]
fn windows_containment_child() {
    let Ok(mode) = std::env::var("HUDSON_CONTAINMENT_TEST_MODE") else {
        return;
    };
    if mode == "parent" {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "windows_containment_child",
                "--nocapture",
            ])
            .env("HUDSON_CONTAINMENT_TEST_MODE", "child")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::fs::write(
            std::env::var("HUDSON_CONTAINMENT_TEST_MARKER").unwrap(),
            child.id().to_string(),
        )
        .unwrap();
    }
    std::thread::sleep(Duration::from_secs(30));
}
#[cfg(windows)]
#[test]
fn windows_deadline_terminates_descendants_in_the_assigned_job() {
    use std::os::windows::process::CommandExt;
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("descendant");
    std::env::set_var("HUDSON_CONTAINMENT_TEST_MODE", "parent");
    std::env::set_var("HUDSON_CONTAINMENT_TEST_MARKER", &marker);
    let settings:BridgeConfig=serde_json::from_value(json!({"id":"fixture","version":1,"timeout_seconds":1,
        "transport":{"type":"stdio","executable":std::env::current_exe().unwrap(),
        "args":["--ignored","--exact","windows_containment_child","--nocapture"],"cwd":directory.path(),
        "env":["HUDSON_CONTAINMENT_TEST_MODE","HUDSON_CONTAINMENT_TEST_MARKER"]}})).unwrap();
    let mut model = BridgeModel::new(settings, 16).unwrap();
    std::env::remove_var("HUDSON_CONTAINMENT_TEST_MODE");
    std::env::remove_var("HUDSON_CONTAINMENT_TEST_MARKER");
    assert!(matches!(
        model.call(&request()),
        Err(ExecutionError::Unknown(_))
    ));
    let pid = std::fs::read_to_string(marker).unwrap();
    let started = std::time::Instant::now();
    loop {
        let output = std::process::Command::new("tasklist.exe")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        let alive = text.lines().any(|line| {
            line.split(',')
                .nth(1)
                .is_some_and(|value| value.trim_matches('"') == pid)
        });
        if !alive {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "contained descendant remained alive"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
