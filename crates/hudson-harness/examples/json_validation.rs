//! Reproduce the JSON-validation workload used by sandbox request preparation.
use std::{hint::black_box, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "ignored".into());
    if !matches!(mode.as_str(), "value" | "ignored") {
        return Err("usage: json_validation [value|ignored]".into());
    }
    let body = format!(
        "[{}]",
        vec![r#"{"id":42,"text":"bounded payload","ok":true}"#; 1024].join(",")
    );
    assert!(body.len() <= 64 * 1024);
    let iterations = 1000;
    let started = Instant::now();
    for _ in 0..iterations {
        if mode == "value" {
            black_box(serde_json::from_str::<serde_json::Value>(black_box(&body))?);
        } else {
            black_box(serde_json::from_str::<serde::de::IgnoredAny>(black_box(
                &body,
            ))?);
        }
    }
    println!(
        "mode={mode} bytes={} iterations={iterations} elapsed_us={}",
        body.len(),
        started.elapsed().as_micros()
    );
    Ok(())
}
