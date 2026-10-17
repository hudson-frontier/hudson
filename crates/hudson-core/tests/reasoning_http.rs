use hudson_core::adapters::{
    anthropic::AnthropicModel,
    chat::ChatModel,
    models::ModelExecutor,
    reasoning::{Effort, Reasoning},
};
use hudson_harness::ModelRequest;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::{Duration, Instant},
};

#[test]
fn reasoning_controls_reach_each_transport_without_changing_output_caps() {
    for case in 0..4 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/model", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let started = Instant::now();
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && started.elapsed() < Duration::from_secs(5) =>
                    {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                assert!(headers.len() < 8192);
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap().to_ascii_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse()
                .unwrap();
            assert!(length <= 65_536);
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["model"], "fixture-model");
            match case {
                0 | 1 => {
                    assert_eq!(body["reasoning_effort"], "low");
                    assert_eq!(
                        body[if case == 0 {
                            "max_completion_tokens"
                        } else {
                            "max_tokens"
                        }],
                        4096
                    );
                    assert!(body.get("thinking").is_none());
                }
                2 => {
                    assert_eq!(body["thinking"], json!({"type":"adaptive"}));
                    assert_eq!(body["output_config"]["effort"], "high");
                    assert_eq!(body["max_tokens"], 4096);
                }
                3 => {
                    assert_eq!(
                        body["thinking"],
                        json!({"type":"enabled","budget_tokens":1024})
                    );
                    assert_eq!(body["max_tokens"], 4096);
                }
                _ => unreachable!(),
            }
            let response = if case < 2 {
                json!({"choices":[{"finish_reason":"stop","message":{"content":"answer"}}],"usage":{"prompt_tokens":10,"completion_tokens":4}})
            } else {
                json!({"stop_reason":"end_turn","content":[{"type":"text","text":"answer"}],"usage":{"input_tokens":10,"output_tokens":4}})
            }.to_string();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        });
        let mut model: Box<dyn ModelExecutor> = match case {
            0 | 1 => Box::new(
                ChatModel::new(&endpoint, None, 4096)
                    .unwrap()
                    .with_legacy_token_limit(case == 1)
                    .with_reasoning(Some(Reasoning::Effort {
                        effort: Effort::Low,
                    }))
                    .unwrap(),
            ),
            2 => Box::new(
                AnthropicModel::new(&endpoint, None, 4096)
                    .unwrap()
                    .with_reasoning(Some(Reasoning::Effort {
                        effort: Effort::High,
                    }))
                    .unwrap(),
            ),
            3 => Box::new(
                AnthropicModel::new(&endpoint, None, 4096)
                    .unwrap()
                    .with_reasoning(Some(Reasoning::Budget { tokens: 1024 }))
                    .unwrap(),
            ),
            _ => unreachable!(),
        };
        model
            .call(&ModelRequest {
                instructions: "finish".into(),
                model: "fixture-model".into(),
                messages: vec![],
                tools: vec![],
            })
            .unwrap();
        assert_eq!(
            model.take_usage(),
            Some(hudson_core::models::TokenUsage {
                input_tokens: 10,
                output_tokens: 4
            })
        );
        server.join().unwrap();
    }
}
