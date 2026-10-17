use hudson_core::{
    adapters::{
        mcp::{discover_tools, register_tools, McpServerConfig, McpToolBinding},
        tools::{ExecutionError, Invocation, ToolExecutor, ToolRegistry},
    },
    models::Effect,
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

struct Server {
    endpoint: String,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(mode: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (counter, done) = (calls.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                }
                let headers = String::from_utf8(headers).unwrap().to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut bytes = vec![0; length];
                socket.read_exact(&mut bytes).unwrap();
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                if mode == "model" {
                    let step = counter.fetch_add(1, Ordering::SeqCst);
                    let content = if step < 3 {
                        let (name, args) = match step {
                            0 => ("load_skill", json!({"name":"analysis"})),
                            1 => (
                                "load_skill",
                                json!({"name":"analysis","resource":"references/rules.md"}),
                            ),
                            _ => ("customer_echo", json!({"message":"hello"})),
                        };
                        json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call-{step}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
                    } else {
                        assert!(request.to_string().contains("frozen-source-token"));
                        json!({"role":"assistant","content":"{\"done\":true}"})
                    };
                    let body = json!({"choices":[{"message":content,"finish_reason":if step < 3 {"tool_calls"} else {"stop"}}]}).to_string();
                    write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                    continue;
                }
                let method = request["method"].as_str().unwrap();
                let result = match method {
                    "initialize" => {
                        json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
                    }
                    "notifications/initialized" => {
                        write!(socket,"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                        continue;
                    }
                    "notifications/cancelled" => {
                        let _ = write!(socket,"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        continue;
                    }
                    "tools/list" => {
                        if request["params"]["cursor"].is_null() {
                            json!({"tools":[], "nextCursor":"second"})
                        } else {
                            json!({"tools":[{"name":"echo","description":"fixture echo","inputSchema":if mode == "drift" { json!({"type":"object"}) } else { schema() }}]})
                        }
                    }
                    "tools/call" => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(request["params"]["arguments"], json!({"message":"hello"}));
                        assert!(request["params"]["_meta"]["hudson/operation_id"].is_string());
                        if mode == "timeout" {
                            std::thread::sleep(Duration::from_millis(1200));
                        }
                        if mode == "disconnect" {
                            continue;
                        }
                        json!({"content":[{"type":"text","text": if matches!(mode, "large" | "sse_large") { "x".repeat(4096) } else { "hello".into() }}],"isError":mode == "error"})
                    }
                    other => panic!("unexpected method {other}"),
                };
                let body = json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
                let (body, content_type) = if matches!(mode, "sse" | "sse_large") {
                    (
                        format!("event: message\ndata: {body}\n\n"),
                        "text/event-stream",
                    )
                } else {
                    (body, "application/json")
                };
                if matches!(mode, "large" | "sse_large") {
                    let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n{body}");
                } else {
                    let _ = write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                }
            }
        });
        Self {
            endpoint,
            calls,
            stop,
            thread: Some(thread),
        }
    }
    fn config(&self) -> McpServerConfig {
        McpServerConfig {
            endpoint: self.endpoint.clone(),
            bearer_env: None,
            timeout_seconds: 1,
            max_response_bytes: 2048,
            tools: vec![McpToolBinding {
                name: "customer_echo".into(),
                remote_name: "echo".into(),
                description: "Echo message".into(),
                input_schema: schema(),
                effect: Effect::Write,
            }],
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn schema() -> Value {
    json!({"type":"object","properties":{"message":{"type":"string"}},"required":["message"],"additionalProperties":false})
}

#[test]
fn initializes_discovers_paginated_tools_and_calls_over_json_and_sse() {
    for mode in ["json", "sse"] {
        let server = Server::new(mode);
        let config = server.config();
        let discovered = discover_tools(&config).unwrap();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].input_schema, schema());
        let mut registry = ToolRegistry::new();
        let tools = register_tools(&config, &mut registry, "workspace", "policy").unwrap();
        assert_eq!(tools[0].name, "customer_echo");
        let output = registry
            .execute(Invocation {
                operation_id: uuid::Uuid::new_v4(),
                tool: &tools[0],
                arguments: &json!({"message":"hello"}),
            })
            .unwrap();
        assert_eq!(output["content"][0]["text"], "hello");
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    }
}
#[test]
fn schema_drift_prevents_dispatch() {
    let server = Server::new("drift");
    let mut registry = ToolRegistry::new();
    let tools = register_tools(&server.config(), &mut registry, "workspace", "policy").unwrap();
    let result = registry.execute(Invocation {
        operation_id: uuid::Uuid::new_v4(),
        tool: &tools[0],
        arguments: &json!({"message":"hello"}),
    });
    assert!(matches!(result, Err(ExecutionError::Failed(_))));
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn interrupted_oversized_or_error_results_are_unknown_and_never_retried() {
    for mode in ["disconnect", "large", "sse_large", "timeout", "error"] {
        let server = Server::new(mode);
        let mut registry = ToolRegistry::new();
        let tools = register_tools(&server.config(), &mut registry, "workspace", "policy").unwrap();
        let result = registry.execute(Invocation {
            operation_id: uuid::Uuid::new_v4(),
            tool: &tools[0],
            arguments: &json!({"message":"hello"}),
        });
        assert!(
            matches!(result, Err(ExecutionError::Unknown(_))),
            "{mode}: {result:?}"
        );
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    }
}
#[test]
fn registration_is_offline_and_pins_configuration() {
    let server = Server::new("json");
    let mut config = server.config();
    config.endpoint = "https://example.invalid/mcp".into();
    let mut registry = ToolRegistry::new();
    let before = register_tools(&config, &mut registry, "workspace", "policy").unwrap();
    config.tools[0].input_schema = json!({"type":"object"});
    let after = register_tools(&config, &mut registry, "workspace", "policy").unwrap();
    assert_ne!(before[0].id, after[0].id);
    config.endpoint = "https://user:secret@example.invalid/mcp".into();
    assert!(register_tools(&config, &mut registry, "workspace", "policy").is_err());
}

#[cfg(feature = "fixtures")]
#[test]
fn runtime_approval_and_unknown_write_preserve_operation_authority() {
    use hudson_core::{
        adapters::models::ModelExecutor, fixtures, models::*, runtime::Runtime, security::Policy,
        storage::MemoryStore,
    };
    use hudson_harness::{ModelRequest, ModelResponse, ToolCall};
    struct Model;
    impl ModelExecutor for Model {
        fn call(&mut self, _: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
            Ok(ModelResponse::ToolCalls {
                calls: vec![ToolCall {
                    call_id: "call-1".into(),
                    name: "customer_echo".into(),
                    arguments: json!({"message":"hello"}),
                    provider_metadata: Value::Null,
                }],
            })
        }
    }
    let server = Server::new("disconnect");
    let mut registry = ToolRegistry::new();
    let tools = register_tools(&server.config(), &mut registry, "demo", "mcp-approval").unwrap();
    let (mut agent, _) = fixtures::definitions();
    agent.tools = vec![AgentTool {
        tool_ref: tools[0].reference(),
        alias: tools[0].name.clone(),
    }];
    agent.input_schema = None;
    let store = MemoryStore::default();
    store.publish_tool(tools[0].clone()).unwrap();
    store.publish_agent(agent.clone()).unwrap();
    store
        .set_policy(
            "demo",
            "mcp-approval",
            Policy {
                actors: ["developer".into()].into(),
                approvers: ["developer".into()].into(),
                require_approval: true,
            },
        )
        .unwrap();
    let actor = fixtures::actor();
    let mut runtime = Runtime::new(store.clone(), fixtures::FixtureBackend, Model, registry);
    let run = runtime
        .submit(&actor, agent.reference(), json!({}), None)
        .unwrap();
    let mut operation = None;
    for _ in 0..10 {
        if let Some(WaitReason::Approval { operation_id }) = runtime.tick(&actor, run).unwrap().wait
        {
            operation = Some(operation_id);
            break;
        }
    }
    let operation = operation.expect("MCP write must wait for approval");
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
    runtime
        .approve(&actor, operation, true, now(), now() + 60_000)
        .unwrap();
    for _ in 0..10 {
        runtime.tick(&actor, run).unwrap();
    }
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    let op = store
        .operations(&actor, run)
        .unwrap()
        .into_iter()
        .find(|op| op.meta.id == operation)
        .unwrap();
    assert_eq!(op.status, OperationStatus::Unknown);
    assert_eq!(op.attempts.len(), 1);
}

#[test]
fn configured_temporal_tree_loads_frozen_packages_and_approves_customer_mcp_tools() {
    use hudson_core::{configured::Configuration, models::*, storage::Store};
    for (server_approval, tool_approval, should_wait) in [
        (None, None, true),
        (Some(false), None, false),
        (Some(false), Some(true), true),
    ] {
        let server = Server::new("json");
        let model = Server::new("model");
        let root =
            std::env::temp_dir().join(format!("hudson-configured-mcp-{}", uuid::Uuid::new_v4()));
        let package = root.join("analysis");
        std::fs::create_dir_all(package.join("references")).unwrap();
        std::fs::write(package.join("SKILL.md"),"---\nname: analysis\ndescription: Analyze data\n---\nRead references/rules.md before executing tools.").unwrap();
        std::fs::write(package.join("references/rules.md"), "frozen-source-token").unwrap();
        let mut mcp = serde_json::to_value(server.config()).unwrap();
        if let Some(value) = server_approval {
            mcp["require_approval"] = json!(value);
        }
        if let Some(value) = tool_approval {
            mcp["tools"][0]["require_approval"] = json!(value);
        }
        let config = json!({"name":"configured","instructions":"Load analysis skill, read rules, echo hello, and return done.","model":"fixture","endpoint":model.endpoint,"skill_packages":["analysis"],"mcp_servers":[mcp]});
        let path = root.join("agent.json");
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let configuration = Configuration::load(&path).unwrap();
        std::fs::remove_dir_all(&package).unwrap(); // Build must use the frozen package, not reload it.
        let store = Store::default();
        let actor = Actor {
            workspace_id: "customer".into(),
            id: "owner".into(),
        };
        let mut tree = configuration
            .build_temporal_tree(store.clone(), &actor)
            .unwrap();
        assert_eq!(server.calls.load(Ordering::SeqCst), 0);
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        let run = tree
            .runtime
            .submit(
                &actor,
                tree.reference.clone(),
                json!({"task":"analyze"}),
                None,
            )
            .unwrap();
        let mut waited = false;
        let mut completed = false;
        for _ in 0..40 {
            let view = tree.tick(&actor, run).unwrap();
            if let Some(WaitReason::Approval { operation_id }) = view.wait {
                assert!(should_wait);
                assert_eq!(server.calls.load(Ordering::SeqCst), 0);
                waited = true;
                tree.runtime
                    .approve(&actor, operation_id, true, now(), now() + 60_000)
                    .unwrap();
            }
            if view.status == RunStatus::Completed {
                completed = true;
                break;
            }
        }
        assert!(completed);
        assert_eq!(waited, should_wait);
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
        assert_eq!(model.calls.load(Ordering::SeqCst), 4);
        std::fs::remove_dir_all(root).unwrap();
    }
}
