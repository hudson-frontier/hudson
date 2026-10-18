use hudson_core::adapters::{
    anthropic::AnthropicModel, chat::ChatModel, models::ModelExecutor, tools::ExecutionError,
};
use hudson_harness::ModelRequest;
use serde_json::json;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant},
};

fn accept(listener: &TcpListener) -> TcpStream {
    let start = Instant::now();
    loop {
        match listener.accept() {
            Ok((socket, _)) => return socket,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && start.elapsed() < Duration::from_secs(5) =>
            {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("fixture accept failed: {error}"),
        }
    }
}

fn respond(listener: &TcpListener, status: u16, body: &str) {
    let mut socket = accept(listener);
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 8192);
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let headers = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .unwrap()
        .parse()
        .unwrap();
    assert!(length <= 65536);
    let mut request = vec![0; length];
    socket.read_exact(&mut request).unwrap();
    // Clients may drop an error response without reading its entire body.
    let _ = write!(socket, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}

#[test]
fn both_transports_classify_errors_without_leaking_bodies_usage_or_retrying() {
    let cases = [
        (400, json!({"error":{"code":"context_length_exceeded","message":"private-prompt"}}).to_string(), "context_limit"),
        (400, json!({"error":{"code":"private-code","type":"private-type","message":"context_length_exceeded private-prompt"}}).to_string(), "invalid_request"),
        (401, json!({"error":{"code":"context_length_exceeded","message":"private-key"}}).to_string(), "authentication_failed"),
        (402, "private-billing-body".into(), "billing_failed"),
        (403, "private-body".into(), "permission_denied"),
        (404, "private-body".into(), "resource_not_found"),
        (413, "private-body".into(), "request_too_large"),
        (429, json!({"error":{"code":"credit_balance_exhausted","message":"private-account"},"usage":{"prompt_tokens":999,"completion_tokens":999}}).to_string(), "quota_exhausted"),
        (429, json!({"error":{"code":"context_length_exceeded","message":"private-prompt"}}).to_string(), "rate_limited"),
        (429, "not JSON private-body".into(), "rate_limited"),
        (400, format!("{}{}", json!({"error":{"code":"context_length_exceeded"}}), " ".repeat(32769)), "invalid_request"),
        (503, "private-body".into(), "provider_unavailable"),
        (504, "private-body".into(), "request_timeout"),
        (529, "private-body".into(), "provider_unavailable"),
    ];
    for native in [false, true] {
        for (status, body, category) in &cases {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}/model", listener.local_addr().unwrap());
            let body = body.clone();
            let status = *status;
            let server = std::thread::spawn(move || {
                let success = if native {
                    json!({"stop_reason":"end_turn","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":12,"output_tokens":3}})
                } else {
                    json!({"choices":[{"finish_reason":"stop","message":{"content":"hello"}}],"usage":{"prompt_tokens":12,"completion_tokens":3}})
                }.to_string();
                respond(&listener, 200, &success);
                respond(&listener, status, &body);
                std::thread::sleep(Duration::from_millis(20));
                assert!(
                    matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
                );
            });
            let mut model: Box<dyn ModelExecutor> = if native {
                Box::new(AnthropicModel::new(&endpoint, Some("private-key".into()), 64).unwrap())
            } else {
                Box::new(ChatModel::new(&endpoint, Some("private-key".into()), 64).unwrap())
            };
            let request = ModelRequest {
                instructions: "test".into(),
                model: "fixture".into(),
                messages: vec![],
                tools: vec![],
            };
            model.call(&request).unwrap();
            // Deliberately leave the first usage report unconsumed.
            let error = model.call(&request).unwrap_err();
            let ExecutionError::Failed(message) = error else {
                panic!("definitive HTTP failure expected")
            };
            assert_eq!(message, format!("model HTTP status {status}: {category}"));
            assert!(!message.contains("private"));
            assert!(model.take_usage().is_none());
            assert!(model.take_usage().is_none());
            server.join().unwrap();
        }
    }
}

#[test]
fn interrupted_transport_stays_unknown_without_echoing_the_endpoint_or_key() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/private-path", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        drop(accept(&listener));
    });
    let mut model = ChatModel::new(&endpoint, Some("private-key".into()), 64).unwrap();
    let error = model
        .call(&ModelRequest {
            instructions: "private-prompt".into(),
            model: "fixture".into(),
            messages: vec![],
            tools: vec![],
        })
        .unwrap_err();
    assert!(matches!(error, ExecutionError::Unknown(message) if !message.contains("private")));
    assert!(model.take_usage().is_none());
    server.join().unwrap();
}
