//! Offline comparison of an SDK bridge and the existing direct model transport.
//! Driven by scripts/bench_runtime_bridge.py; literal loopback endpoints only.
use hudson_core::adapters::{
    chat::ChatModel,
    models::ModelExecutor,
    runtime_bridge::{BridgeConfig, BridgeModel, BridgeTransport},
};
use hudson_harness::{Content, Message, ModelRequest, ModelResponse, Role};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 3 {
        return Err("expected MODE LOOPBACK_ENDPOINT CALLS".into());
    }
    let endpoint = reqwest::Url::parse(&args[1])?;
    if endpoint.scheme() != "http" || !matches!(endpoint.host_str(), Some("127.0.0.1" | "[::1]")) {
        return Err("benchmark requires a literal loopback HTTP fixture".into());
    }
    let calls = args[2].parse::<usize>()?;
    if !(1..=10000).contains(&calls) {
        return Err("calls require 1..=10000".into());
    }
    let mut model: Box<dyn ModelExecutor> = match args[0].as_str() {
        "direct" | "image" => Box::new(ChatModel::new(&args[1], None, 32)?),
        "bridge" | "stream" => Box::new(BridgeModel::new(
            BridgeConfig {
                id: "benchmark".into(),
                version: 1,
                transport: BridgeTransport::Http {
                    endpoint: args[1].clone(),
                    bearer_env: None,
                },
                timeout_seconds: 10,
                max_request_bytes: 65536,
                max_response_bytes: 65536,
                stream: args[0] == "stream",
                images: false,
                native_sessions: false,
            },
            32,
        )?),
        _ => return Err("mode must be direct, bridge, stream or image".into()),
    };
    let mut content = vec![Content::Text {
        text: "x".repeat(1024),
    }];
    let image_bytes = if args[0] == "image" {
        2 * 1024 * 1024 / 3 * 3
    } else {
        0
    };
    if image_bytes != 0 {
        content.push(Content::Image {
            image: hudson_harness::images::Image {
                media_type: "image/png".into(),
                data: "QUFB".repeat(image_bytes / 3),
            },
        });
    }
    let request = ModelRequest {
        instructions: "Return 42".into(),
        model: "offline-fixture".into(),
        messages: vec![Message {
            role: Role::User,
            content,
        }],
        tools: vec![],
    };
    let mut timings = Vec::with_capacity(calls);
    for _ in 0..calls {
        let start = std::time::Instant::now();
        let response = model.call(&request).map_err(|error| format!("{error:?}"))?;
        if response != (ModelResponse::Final { output: json!(42) }) {
            return Err("unexpected fixture result".into());
        }
        timings.push(start.elapsed().as_micros());
        model.take_usage();
    }
    let total = timings.iter().sum::<u128>();
    timings.sort_unstable();
    println!(
        "{}",
        json!({"mode":args[0],"calls":calls,"input_text_bytes":1024,"input_image_bytes":image_bytes,"peak_rss_kib":peak_rss(),"p50_us":timings[(calls-1)/2],
        "p95_us":timings[(calls-1)*95/100],"total_us":total})
    );
    Ok(())
}

fn peak_rss() -> Option<u64> {
    use std::io::Read;
    let mut status = String::new();
    std::fs::File::open("/proc/self/status")
        .ok()?
        .take(16384)
        .read_to_string(&mut status)
        .ok()?;
    status
        .lines()
        .find(|line| line.starts_with("VmHWM:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}
