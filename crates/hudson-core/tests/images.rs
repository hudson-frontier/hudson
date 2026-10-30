#![cfg(feature = "fixtures")]
use hudson_core::{
    adapters::{
        anthropic::AnthropicModel,
        chat::ChatModel,
        models::ModelExecutor,
        runtime_bridge::{BridgeConfig, BridgeModel},
        tools::ExecutionError,
    },
    fixtures::{self, FixtureTools},
    models::*,
    multimodal::{Image, ImageTurn},
    runtime::Runtime,
    security::Policy,
    storage::Store,
};
use hudson_harness::{AgentLoop, Content, Message, ModelRequest, Role};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn image() -> Image {
    Image {media_type:"image/png".into(),data:"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jCD8AAAAASUVORK5CYII=".into()}
}
fn serve(anthropic: bool) -> (String, Arc<Mutex<Vec<Value>>>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/model", listener.local_addr().unwrap());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let requests = observed.clone();
    let peer = std::thread::spawn(move || {
        let start = Instant::now();
        for index in 0..2 {
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(start.elapsed() < Duration::from_secs(15));
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = Vec::new();
            let end = loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).unwrap();
                assert!(n > 0 && bytes.len() + n <= 1024 * 1024);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(pos) = bytes.windows(4).position(|x| x == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let header = std::str::from_utf8(&bytes[..end]).unwrap();
            let length = header
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .unwrap()
                .1
                .trim()
                .parse::<usize>()
                .unwrap();
            assert!(length <= 1024 * 1024);
            while bytes.len() < end + length {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            requests
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&bytes[end..end + length]).unwrap());
            let response = if anthropic {
                if index == 0 {
                    json!({"content":[{"type":"tool_use","id":"lookup-1","name":"lookup_order","input":{"order_id":"123"}}],"stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":2}})
                } else {
                    json!({"content":[{"type":"text","text":"42"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":2}})
                }
            } else if index == 0 {
                json!({"choices":[{"finish_reason":"tool_calls","message":{"content":"Checking the image context","tool_calls":[{"id":"lookup-1","type":"function","function":{"name":"lookup_order","arguments":"{\"order_id\":\"123\"}"}}]}}],"usage":{"prompt_tokens":10,"completion_tokens":2}})
            } else {
                json!({"choices":[{"finish_reason":"stop","message":{"content":"42"}}],"usage":{"prompt_tokens":10,"completion_tokens":2}})
            };
            let body = response.to_string();
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
        }
    });
    (endpoint, observed, peer)
}
fn install(store: &Store) {
    let (mut agent, tools) = fixtures::definitions();
    agent.input_schema = None;
    agent.output_schema = None;
    for tool in tools {
        store
            .set_policy(
                "demo",
                &tool.policy_ref,
                Policy {
                    actors: ["developer".into()].into(),
                    approvers: ["developer".into()].into(),
                    require_approval: tool.effect == Effect::Write,
                },
            )
            .unwrap();
        store.publish_tool(tool).unwrap();
    }
    store.publish_agent(agent).unwrap();
}
fn model(anthropic: bool, endpoint: &str) -> Box<dyn ModelExecutor> {
    if anthropic {
        Box::new(AnthropicModel::new(endpoint, None, 16).unwrap())
    } else {
        Box::new(ChatModel::new(endpoint, None, 16).unwrap())
    }
}
fn cycle(store: Store, anthropic: bool, reopen: impl FnOnce() -> Store) {
    install(&store);
    let (endpoint, requests, peer) = serve(anthropic);
    let actor = fixtures::actor();
    let tools = FixtureTools::default();
    let effects = tools.executed.clone();
    let mut runtime = Runtime::new(
        store.clone(),
        AgentLoop,
        model(anthropic, &endpoint),
        tools.clone(),
    );
    let turn = || ImageTurn {
        input: json!("Describe the image"),
        images: vec![image()],
        request_key: "image-once".into(),
        goal: None,
    };
    let id = runtime
        .submit_images(&actor, fixtures::agent_ref(), turn(), None)
        .unwrap();
    assert_eq!(
        runtime
            .submit_images(&actor, fixtures::agent_ref(), turn(), None)
            .unwrap(),
        id
    );
    let mut changed = turn();
    changed.images[0].data = "YQ==".into();
    assert!(runtime
        .submit_images(&actor, fixtures::agent_ref(), changed, None)
        .is_err());
    runtime.tick(&actor, id).unwrap();
    runtime.tick(&actor, id).unwrap();
    drop(runtime);
    let reopened = reopen();
    let mut runtime = Runtime::new(reopened, AgentLoop, model(anthropic, &endpoint), tools);
    let view = fixtures::drive(&mut runtime, &actor, id).unwrap();
    assert_eq!(view.status, RunStatus::Completed);
    assert_eq!(view.result, Some(json!(42)));
    assert_eq!(view.usage.model_calls, 2);
    assert_eq!(view.usage.reported_input_tokens, 20);
    assert_eq!(effects.lock().unwrap().len(), 1);
    let transcript = runtime.store.export_transcript(&actor, id).unwrap();
    assert_eq!(transcript.format_version, 2);
    let images = transcript
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| {
            if let Content::Image { image } = c {
                Some(image)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(images, vec![&image()]);
    peer.join().unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        let images = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .filter(|b| b["type"] == "image" || b["type"] == "image_url")
            .collect::<Vec<_>>();
        assert_eq!(images.len(), 1);
        if anthropic {
            assert_eq!(images[0]["source"]["data"], image().data);
        } else {
            assert_eq!(
                images[0]["image_url"]["url"],
                format!("data:image/png;base64,{}", image().data)
            );
        }
    }
}
#[test]
fn chat_and_anthropic_preserve_images_across_tool_cycles_and_portable_export() {
    for anthropic in [false, true] {
        let store = Store::default();
        cycle(store.clone(), anthropic, || store);
    }
}
#[test]
#[ignore = "requires dedicated PostgreSQL database on /tmp"]
fn postgres_image_input_reopens_without_repeating_model_or_tool_effects() {
    let database = std::env::var("HUDSON_TEST_DATABASE").expect("dedicated test database");
    let namespace = format!("images-{}", uuid::Uuid::new_v4());
    cycle(
        Store::postgres_local("/tmp", &database, &namespace).unwrap(),
        false,
        || Store::postgres_local("/tmp", &database, &namespace).unwrap(),
    );
}
#[test]
fn bridges_reject_images_before_io_unless_the_operator_opts_in() {
    let config:BridgeConfig=serde_json::from_value(json!({"id":"image-peer","version":1,"transport":{"type":"http","endpoint":"http://127.0.0.1:1/model"}})).unwrap();
    assert!(!config.descriptor().capabilities.images);
    let mut model = BridgeModel::new(config, 16).unwrap();
    let request = ModelRequest {
        instructions: "describe".into(),
        model: "fixture".into(),
        tools: vec![],
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Image { image: image() }],
        }],
    };
    assert!(matches!(
        model.call(&request),
        Err(ExecutionError::Failed(_))
    ));
    assert!(model.take_usage().is_none());
}
