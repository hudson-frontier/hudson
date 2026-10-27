//! Bounded Anthropic SSE assembly. Partial arguments never leave this module.
use super::tools::ExecutionError;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read},
    sync::atomic::{AtomicBool, Ordering},
};
fn unknown() -> ExecutionError {
    ExecutionError::Unknown("model stream interrupted or malformed; response is unavailable".into())
}

fn append(block: &mut Value, field: &str, value: &str) -> Result<(), ExecutionError> {
    let Value::String(text) = &mut block[field] else {
        return Err(unknown());
    };
    text.push_str(value);
    Ok(())
}
pub(super) fn read(
    reader: impl Read,
    observer: &mut dyn FnMut(&str),
    cancelled: &AtomicBool,
) -> Result<Value, ExecutionError> {
    let mut reader = BufReader::new(reader.take(2_097_153));
    let mut total = 0;
    let mut data = Vec::new();
    let mut message = Value::Null;
    let mut blocks: Vec<Value> = Vec::new();
    let mut arguments: BTreeMap<usize, String> = BTreeMap::new();
    let mut active = None;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(unknown());
        }
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(131_073)
            .read_until(b'\n', &mut line)
            .map_err(|_| unknown())?;
        total += count;
        if count == 0 || count > 131_072 || total > 2_097_152 {
            return Err(unknown());
        }
        let line = std::str::from_utf8(&line)
            .map_err(|_| unknown())?
            .trim_end_matches(['\r', '\n']);
        if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(value.trim_start().as_bytes());
            continue;
        }
        if !line.is_empty() || data.is_empty() {
            continue;
        }
        let event: Value =
            serde_json::from_slice(&std::mem::take(&mut data)).map_err(|_| unknown())?;
        match event["type"].as_str() {
            Some("ping") => {}
            Some("message_start") if message.is_null() => {
                message = event["message"].clone();
                if !message.is_object()
                    || !message["usage"].is_object()
                    || !message["content"].as_array().is_some_and(Vec::is_empty)
                {
                    return Err(unknown());
                }
            }
            Some("content_block_start") if !message.is_null() && active.is_none() => {
                let index = event["index"].as_u64().ok_or_else(unknown)? as usize;
                if index != blocks.len() || blocks.len() >= 64 {
                    return Err(unknown());
                }
                blocks.push(event["content_block"].clone());
                active = Some(index);
            }
            Some("content_block_delta") => {
                let index = event["index"].as_u64().ok_or_else(unknown)? as usize;
                if active != Some(index) {
                    return Err(unknown());
                }
                let delta = &event["delta"];
                let block = blocks.get_mut(index).ok_or_else(unknown)?;
                match delta["type"].as_str() {
                    Some("input_json_delta") if block["type"] == "tool_use" => arguments
                        .entry(index)
                        .or_default()
                        .push_str(delta["partial_json"].as_str().ok_or_else(unknown)?),
                    Some("text_delta") if block["type"] == "text" => {
                        let text = delta["text"].as_str().ok_or_else(unknown)?;
                        append(block, "text", text)?;
                        observer(text);
                    }
                    Some("thinking_delta") if block["type"] == "thinking" => {
                        append(
                            block,
                            "thinking",
                            delta["thinking"].as_str().ok_or_else(unknown)?,
                        )?;
                    }
                    Some("signature_delta") if block["type"] == "thinking" => {
                        append(
                            block,
                            "signature",
                            delta["signature"].as_str().ok_or_else(unknown)?,
                        )?;
                    }
                    _ => return Err(unknown()),
                }
            }
            Some("content_block_stop") => {
                let index = event["index"].as_u64().ok_or_else(unknown)? as usize;
                if active != Some(index) {
                    return Err(unknown());
                }
                if let Some(input) = arguments.remove(&index) {
                    blocks[index]["input"] = serde_json::from_str(&input).map_err(|_| unknown())?;
                }
                active = None;
            }
            Some("message_delta") if !message.is_null() && active.is_none() => {
                message["stop_reason"] = event["delta"]["stop_reason"].clone();
                if let Some(usage) = event["usage"].as_object() {
                    for (key, value) in usage {
                        message["usage"][key] = value.clone();
                    }
                }
            }
            Some("message_stop")
                if !message.is_null() && active.is_none() && message["stop_reason"].is_string() =>
            {
                message["content"] = json!(blocks);
                return Ok(message);
            }
            _ => return Err(unknown()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn events(values: Vec<Value>) -> Vec<u8> {
        values
            .into_iter()
            .map(|value| {
                format!(
                    "event: {}\ndata: {value}\n\n",
                    value["type"].as_str().unwrap()
                )
            })
            .collect::<String>()
            .into_bytes()
    }
    fn start() -> Value {
        json!({"type":"message_start","message":{"content":[],"usage":{"input_tokens":8,"output_tokens":0}}})
    }
    fn finish(reason: &str) -> Vec<Value> {
        vec![
            json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ]
    }
    #[test]
    fn streamed_text_and_complete_tool_arguments_are_assembled() {
        let mut values = vec![
            start(),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Inspecting"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"read-1","name":"read_file","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}),
            json!({"type":"content_block_stop","index":1}),
        ];
        values.extend(finish("tool_use"));
        let mut text = String::new();
        let value = read(
            events(values).as_slice(),
            &mut |part| text.push_str(part),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(text, "Inspecting");
        assert_eq!(value["content"][1]["input"], json!({"path":"a.txt"}));
        assert_eq!(value["usage"]["input_tokens"], 8);
        assert_eq!(value["usage"]["output_tokens"], 3);
    }
    #[test]
    fn partial_or_invalid_streams_never_produce_executable_responses() {
        let partial = events(vec![
            start(),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"write","name":"shell","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"write"}}),
        ]);
        assert!(matches!(
            read(partial.as_slice(), &mut |_| {}, &AtomicBool::new(false)),
            Err(ExecutionError::Unknown(_))
        ));
        for data in [
            events(vec![
                json!({"type":"message_start","message":{"content":[],"usage":0}}),
            ]),
            vec![b'x'; 131_073],
        ] {
            assert!(read(data.as_slice(), &mut |_| {}, &AtomicBool::new(false)).is_err());
        }
        assert!(read(
            events(vec![start()]).as_slice(),
            &mut |_| {},
            &AtomicBool::new(true)
        )
        .is_err());
    }
}
