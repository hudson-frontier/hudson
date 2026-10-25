#![cfg(feature = "fixtures")]
use hudson_core::{
    adapters::{
        chat::ChatModel,
        http_tools::HttpTools,
        mcp::{discover_tools, McpServerConfig},
        models::ModelExecutor,
        tools::{Invocation, ToolExecutor, ToolRegistry},
    },
    fixtures,
    models::Execution,
};
use hudson_harness::ModelRequest;
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const TOKEN: &str = "loopback-transport-fixture-token";
const CHILD: &str = "HUDSON_LOOPBACK_TRANSPORT_CHILD";
const ENDPOINT: &str = "HUDSON_LOOPBACK_TRANSPORT_ENDPOINT";
const KEY: &str = "HUDSON_LOOPBACK_TRANSPORT_TOKEN";

struct Server {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (received, done) = (requests.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture listener failed: {error}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") && head.len() < 16384 {
                    let mut byte = [0];
                    if socket.read_exact(&mut byte).is_err() {
                        break;
                    }
                    head.push(byte[0]);
                }
                if !head.ends_with(b"\r\n\r\n") {
                    continue;
                }
                let mut requests = received.lock().unwrap();
                if requests.len() < 32 {
                    requests.push(String::from_utf8(head).unwrap().to_lowercase());
                }
                drop(requests);
                let _ = socket.write_all(
                    b"HTTP/1.1 503 Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        Self {
            port,
            requests,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn invoke(kind: &str, endpoint: &str) {
    assert!(
        endpoint.starts_with("http://127.0.0.1:")
            || endpoint.starts_with("http://localhost:")
            || endpoint.starts_with("https://127.0.0.1:")
            || endpoint.starts_with("https://localhost:")
    );
    match kind {
        "tool" => {
            let (_, mut definitions) = fixtures::definitions();
            let mut tool = definitions.remove(0);
            tool.execution = Execution::Http {
                endpoint: endpoint.into(),
            };
            let mut executor = HttpTools::new(ToolRegistry::new()).unwrap();
            executor.bind(endpoint, Some(TOKEN.into())).unwrap();
            assert!(executor
                .execute(Invocation {
                    operation_id: uuid::Uuid::new_v4(),
                    tool: &tool,
                    arguments: &serde_json::json!({"a":1}),
                })
                .is_err());
        }
        "chat" => {
            let mut model = ChatModel::new(endpoint, Some(TOKEN.into()), 64).unwrap();
            assert!(model
                .call(&ModelRequest {
                    instructions: "fixture".into(),
                    model: "fixture".into(),
                    messages: vec![],
                    tools: vec![]
                })
                .is_err());
        }
        "mcp" => {
            let config = McpServerConfig {
                endpoint: endpoint.into(),
                bearer_env: Some(KEY.into()),
                timeout_seconds: 1,
                max_response_bytes: 4096,
                tools: vec![],
            };
            assert!(discover_tools(&config).is_err());
        }
        _ => panic!("unknown fixture transport"),
    }
}

fn check(kind: &str, test: &str) {
    if std::env::var(CHILD).as_deref() == Ok(kind) {
        invoke(kind, &std::env::var(ENDPOINT).unwrap());
        return;
    }
    for scheme in ["http", "https"] {
        for hostname in ["127.0.0.1", "localhost"] {
            let names = if scheme == "http" {
                ["http_proxy", "all_proxy"]
            } else {
                ["https_proxy", "all_proxy"]
            };
            for variable in names {
                let destination = Server::new();
                let proxy = Server::new();
                let endpoint = format!("{scheme}://{hostname}:{}/fixture", destination.port);
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", test, "--nocapture"])
                    .env(CHILD, kind)
                    .env(ENDPOINT, endpoint)
                    .env(KEY, TOKEN)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                for name in [
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                    "NO_PROXY",
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                    "no_proxy",
                ] {
                    command.env_remove(name);
                }
                command.env(variable, format!("http://127.0.0.1:{}", proxy.port));
                let mut child = command.spawn().unwrap();
                let deadline = Instant::now() + Duration::from_secs(15);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() >= deadline {
                        child.kill().unwrap();
                        child.wait().unwrap();
                        panic!("local {kind} fixture exceeded its deadline");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let proxied = proxy.requests.lock().unwrap().clone();
                let direct = destination.requests.lock().unwrap().clone();
                if scheme == "http" {
                    assert!(
                        proxied.is_empty(),
                        "{kind}: authenticated loopback request reached the proxy"
                    );
                    assert!(
                        direct
                            .iter()
                            .any(|head| head.contains(&format!("authorization: bearer {TOKEN}"))),
                        "{kind}: destination did not receive authentication"
                    );
                } else {
                    assert!(
                        !proxied.is_empty(),
                        "{kind}: HTTPS proxy behavior was disabled"
                    );
                    assert!(proxied
                        .iter()
                        .all(|head| head.starts_with("connect ") && !head.contains(TOKEN)));
                    assert!(direct.is_empty());
                }
            }
        }
    }
}

#[test]
fn http_tool_loopback_requests_ignore_proxies() {
    check("tool", "http_tool_loopback_requests_ignore_proxies");
}
#[test]
fn chat_loopback_requests_ignore_proxies() {
    check("chat", "chat_loopback_requests_ignore_proxies");
}
#[test]
fn mcp_loopback_requests_ignore_proxies() {
    check("mcp", "mcp_loopback_requests_ignore_proxies");
}
