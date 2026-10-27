//! Cursor-based observation only: following never resumes, approves, or cancels a run.
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{io::Write, time::Duration};

pub struct Options {
    pub after: u64,
    pub page_size: u64,
    pub poll_ms: u64,
    pub max_retries: u32,
}

enum FetchError {
    Retryable,
    Fatal(String),
}

fn fetch(client: &Client, url: &str) -> Result<Value, FetchError> {
    use std::io::Read;
    let response = client.get(url).send().map_err(|_| FetchError::Retryable)?;
    let status = response.status();
    if status.is_server_error() || status.as_u16() == 429 {
        return Err(FetchError::Retryable);
    }
    if !status.is_success() {
        // Do not print an error body or URL that could echo authentication data.
        return Err(FetchError::Fatal(format!("Hudson returned {status}")));
    }
    let mut bytes = Vec::new();
    response
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| FetchError::Retryable)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(FetchError::Fatal("follow response exceeds 8 MiB".into()));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| FetchError::Fatal("invalid JSON in follow response".into()))
}

fn fetch_with_retry(
    client: &Client,
    url: &str,
    options: &Options,
    after: u64,
) -> Result<Value, String> {
    for attempt in 0..=options.max_retries {
        match fetch(client, url) {
            Ok(value) => return Ok(value),
            Err(FetchError::Fatal(reason)) => return Err(reason),
            Err(FetchError::Retryable) if attempt < options.max_retries => {
                let delay = options.poll_ms.saturating_mul(1_u64 << attempt).min(30_000);
                eprintln!("follow: retrying unavailable server in {delay} ms; after={after}");
                std::thread::sleep(Duration::from_millis(delay));
            }
            Err(FetchError::Retryable) => return Err("follow retry limit reached".into()),
        }
    }
    unreachable!("bounded retry loop always returns")
}

fn page(value: Value, after: u64, limit: u64, run_id: &str) -> Result<Vec<Value>, String> {
    let events = value.as_array().ok_or("event page must be an array")?;
    if events.len() as u64 > limit {
        return Err("event page exceeds requested limit".into());
    }
    let mut previous = after;
    for event in events {
        let sequence = event["sequence"]
            .as_u64()
            .ok_or("event sequence must be an unsigned integer")?;
        if sequence <= previous || event["run_id"].as_str() != Some(run_id) {
            return Err("event page has invalid ordering or run identity".into());
        }
        previous = sequence;
    }
    Ok(events.clone())
}

fn emit(output: &mut impl Write, record: Value) -> Result<(), String> {
    serde_json::to_writer(&mut *output, &record).map_err(|_| "cannot write follow output")?;
    writeln!(output).map_err(|_| "cannot write follow output")?;
    output
        .flush()
        .map_err(|_| "cannot flush follow output".into())
}

pub fn run(client: &Client, base: &str, run_id: &str, options: Options) -> Result<(), String> {
    let mut output = std::io::stdout().lock();
    observe(client, base, run_id, options, false, |record| {
        emit(&mut output, record)
    })
    .map(|_| ())
}

/// Shared bounded observer; interactive sessions yield at waits without taking action.
pub fn observe(
    client: &Client,
    base: &str,
    run_id: &str,
    options: Options,
    stop_on_wait: bool,
    mut output: impl FnMut(Value) -> Result<(), String>,
) -> Result<u64, String> {
    let mut after = options.after;
    let mut previous_run = Value::Null;
    let result = (|| {
        loop {
            // Observe status BEFORE draining: terminal status guarantees its
            // committed final events exist before the following event requests.
            let run = fetch_with_retry(client, &format!("{base}/runs/{run_id}"), &options, after)?;
            if run["id"]
                .as_str()
                .is_none_or(|id| !id.eq_ignore_ascii_case(run_id))
            {
                return Err("run response has invalid identity".into());
            }
            let status = run["status"].as_str().ok_or("run status is missing")?;
            if !matches!(
                status,
                "queued"
                    | "running"
                    | "waiting"
                    | "cancelling"
                    | "completed"
                    | "failed"
                    | "cancelled"
            ) {
                return Err("unknown run status".into());
            }
            loop {
                let value = fetch_with_retry(
                    client,
                    &format!(
                        "{base}/runs/{run_id}/events?after={after}&limit={}",
                        options.page_size
                    ),
                    &options,
                    after,
                )?;
                let events = page(value, after, options.page_size, run_id)?;
                if events.is_empty() {
                    break;
                }
                for event in events {
                    let sequence = event["sequence"].as_u64().expect("validated sequence");
                    output(json!({"type":"event", "after":sequence, "event":event}))?;
                    after = sequence;
                }
            }
            if run != previous_run {
                output(json!({"type":"run", "after":after, "run":run}))?;
                previous_run = run.clone();
            }
            match status {
                "completed" => return Ok(after),
                "waiting" if stop_on_wait => return Ok(after),
                "failed" | "cancelled" => return Err(format!("run {status}")),
                _ => std::thread::sleep(Duration::from_millis(options.poll_ms)),
            }
        }
    })();
    result.map_err(|reason: String| format!("{reason}; reconnect with --after {after}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_nonmonotonic_and_foreign_pages_before_output() {
        for value in [
            json!([{"sequence":2,"run_id":"a"},{"sequence":2,"run_id":"a"}]),
            json!([{"sequence":1,"run_id":"a"}]),
            json!([{"sequence":2,"run_id":"other"}]),
            json!([{"sequence":-1,"run_id":"a"}]),
            json!({"error":"not a page"}),
        ] {
            assert!(page(value, 1, 2, "a").is_err());
        }
        assert!(page(
            json!([{"sequence":2,"run_id":"a"},{"sequence":3,"run_id":"a"}]),
            1,
            1,
            "a"
        )
        .is_err());
        assert_eq!(
            page(json!([]), u64::MAX, 2, "a").unwrap(),
            Vec::<Value>::new()
        );
    }
}
