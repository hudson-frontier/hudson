//! Native Anthropic Messages API for text and client-side function tools.
use super::{chat::ChatModel, models::ModelExecutor, tools::ExecutionError};
use hudson_harness::{Content, ModelRequest, ModelResponse, Role, ToolCall, ToolOutcome};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
type TextObserver = Box<dyn FnMut(&str) + Send>;
pub struct AnthropicModel {
    transport: ChatModel,
    max_tokens: u32,
    reasoning: Option<super::reasoning::Reasoning>,
    streaming: Option<(TextObserver, Arc<AtomicBool>)>,
}
fn invalid() -> ExecutionError {
    ExecutionError::Failed("unsupported or malformed Anthropic message".into())
}
impl AnthropicModel {
    pub fn new(endpoint: &str, key: Option<String>, max_tokens: u32) -> crate::Result<Self> {
        Ok(Self {
            transport: ChatModel::new(endpoint, key, max_tokens)?,
            max_tokens,
            reasoning: None,
            streaming: None,
        })
    }
    /// Text deltas are display-only. A response becomes executable only after a
    /// complete, validated message_stop. Cancellation never retries a request.
    pub fn with_streaming(
        mut self,
        observer: impl FnMut(&str) + Send + 'static,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        self.streaming = Some((Box::new(observer), cancelled));
        self
    }
    pub fn with_reasoning(
        mut self,
        reasoning: Option<super::reasoning::Reasoning>,
    ) -> crate::Result<Self> {
        if let Some(reasoning) = &reasoning {
            reasoning.validate("anthropic", self.max_tokens)?;
        }
        self.reasoning = reasoning;
        Ok(self)
    }
}
impl ModelExecutor for AnthropicModel {
    fn take_usage(&mut self) -> Option<crate::models::TokenUsage> {
        self.transport.take_usage()
    }
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        self.transport.take_usage();
        let mut body = encode(request, self.max_tokens)?;
        if let Some(reasoning) = &self.reasoning {
            reasoning.apply(&mut body, true);
        }
        let response = if let Some((observer, cancelled)) = &mut self.streaming {
            body["stream"] = json!(true);
            if cancelled.load(Ordering::Relaxed) {
                return Err(ExecutionError::Failed(
                    "cancelled before model dispatch".into(),
                ));
            }
            self.transport
                .post_anthropic_stream(&body, observer, cancelled)?
        } else {
            self.transport.post_json(&body, true)?
        };
        decode(response)
    }
}
fn encode(request: &ModelRequest, max_tokens: u32) -> Result<Value, ExecutionError> {
    hudson_harness::images::validate_history(&request.messages).map_err(|_| invalid())?;
    let mut messages: Vec<Value> = Vec::new();
    for message in &request.messages {
        let role = if message.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        let mut blocks = Vec::new();
        for content in &message.content {
            if let Content::ToolCall { call } = content {
                if let Some(prefix) = call.provider_metadata.get("anthropic_prefix") {
                    blocks.extend(prefix.as_array().ok_or_else(invalid)?.iter().cloned());
                }
            }
            blocks.push(match content {
                Content::Text { text } => json!({"type":"text","text":text}),
                Content::Json { value } => json!({"type":"text","text":value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string())}),
                Content::Image {image} if message.role==Role::User=>json!({"type":"image","source":{"type":"base64","media_type":image.media_type,"data":image.data}}),
                Content::ToolCall { call } if message.role==Role::Assistant => json!({"type":"tool_use","id":call.call_id,"name":call.name,"input":call.arguments}),
                Content::ToolResult { result } if message.role==Role::Tool => json!({"type":"tool_result","tool_use_id":result.call_id,"is_error":matches!(result.outcome,ToolOutcome::Error { .. }),"content":serde_json::to_string(&result.outcome).map_err(|_|invalid())?}),
                _ => return Err(invalid()),
            });
            if let Content::ToolCall { call } = content {
                if let Some(suffix) = call.provider_metadata.get("anthropic_suffix") {
                    blocks.extend(suffix.as_array().ok_or_else(invalid)?.iter().cloned());
                }
            }
        }
        // Parallel tool results must share one user turn.
        if let Some(previous) = messages.last_mut().filter(|m| m["role"] == role) {
            previous["content"]
                .as_array_mut()
                .ok_or_else(invalid)?
                .extend(blocks);
        } else {
            messages.push(json!({"role":role,"content":blocks}));
        }
    }
    let mut body = json!({"model":request.model,"system":request.instructions,"messages":messages,"max_tokens":max_tokens});
    if !request.tools.is_empty() {
        body["tools"] = json!(request
            .tools
            .iter()
            .map(
                |t| json!({"name":t.name,"description":t.description,"input_schema":t.input_schema})
            )
            .collect::<Vec<_>>());
    }
    Ok(body)
}
fn decode(value: Value) -> Result<ModelResponse, ExecutionError> {
    let blocks = value["content"].as_array().ok_or_else(invalid)?;
    let mut calls = Vec::new();
    let mut text = Vec::new();
    let mut pending = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                text.push(block["text"].as_str().ok_or_else(invalid)?.to_owned());
                pending.push(block.clone());
            }
            Some("thinking") if block["thinking"].is_string() && block["signature"].is_string() => {
                pending.push(block.clone())
            }
            Some("redacted_thinking") if block["data"].is_string() => pending.push(block.clone()),
            Some("tool_use") => {
                let field = |key: &str| {
                    block[key]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(invalid)
                };
                if !block["input"].is_object() {
                    return Err(invalid());
                }
                calls.push(ToolCall {
                    provider_metadata: json!({"anthropic_prefix":std::mem::take(&mut pending)}),
                    call_id: field("id")?,
                    name: field("name")?,
                    arguments: block["input"].clone(),
                });
            }
            // Unknown provider blocks require explicit adapter support.
            _ => return Err(invalid()),
        }
    }
    match value["stop_reason"].as_str() {
        Some("tool_use") if !calls.is_empty() => {
            if !pending.is_empty() {
                calls.last_mut().expect("nonempty").provider_metadata["anthropic_suffix"] =
                    json!(pending);
            }
            Ok(ModelResponse::ToolCalls { calls })
        }
        Some("end_turn") if calls.is_empty() && !text.is_empty() => {
            let text = text.join("\n");
            Ok(ModelResponse::Final {
                output: serde_json::from_str(&text).unwrap_or_else(|_| json!(text)),
            })
        }
        _ => Err(invalid()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use hudson_harness::{Message, ToolResult};
    #[test]
    fn groups_tool_results_and_preserves_error_state() {
        let result = |id: &str| Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                result: ToolResult {
                    call_id: id.into(),
                    outcome: ToolOutcome::Error {
                        code: "denied".into(),
                        message: "not allowed".into(),
                    },
                },
            }],
        };
        let request = ModelRequest {
            instructions: "help".into(),
            model: "configured".into(),
            messages: vec![result("a"), result("b")],
            tools: vec![],
        };
        let body = encode(&request, 100).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][0]["content"][0]["is_error"], true);
    }
    #[test]
    fn decodes_tools_and_rejects_truncation_or_opaque_blocks() {
        assert!(matches!(decode(json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"a","name":"query","input":{}}]})).unwrap(),ModelResponse::ToolCalls { .. }));
        assert!(decode(
            json!({"stop_reason":"max_tokens","content":[{"type":"text","text":"partial"}]})
        )
        .is_err());
        assert!(decode(
            json!({"stop_reason":"end_turn","content":[{"type":"thinking","thinking":"opaque"}]})
        )
        .is_err());
    }
}

#[cfg(test)]
mod continuation_tests {
    use super::*;
    use hudson_harness::{Action, AgentLoop, Config, Engine, Input, ToolResult};
    #[test]
    fn preserves_signed_blocks_and_text_order_across_tool_checkpoint() {
        let original = json!([
            {"type":"thinking","thinking":"provider thought","signature":"opaque-signature"},
            {"type":"text","text":"First tool"},
            {"type":"tool_use","id":"a","name":"query","input":{}},
            {"type":"redacted_thinking","data":"opaque-data"},
            {"type":"tool_use","id":"b","name":"query","input":{}},
            {"type":"text","text":"Trailing explanation"}
        ]);
        let response = decode(json!({"stop_reason":"tool_use","content":original})).unwrap();
        let engine = Engine::new(AgentLoop);
        let config = Config {
            model: "claude-configured".into(),
            allow_user_input: false,
            instructions: "help".into(),
            tools: vec![],
            max_context_bytes: 10000,
        };
        let start = engine
            .advance(
                &config,
                &engine.initial_state(),
                Input::Start {
                    value: json!("task"),
                },
            )
            .unwrap();
        let tools = engine
            .advance(&config, &start.checkpoint, Input::Model { response })
            .unwrap();
        let checkpoint =
            serde_json::from_value(serde_json::to_value(tools.checkpoint).unwrap()).unwrap();
        let next = engine
            .advance(
                &config,
                &checkpoint,
                Input::Tools {
                    results: ["a", "b"]
                        .iter()
                        .map(|id| ToolResult {
                            call_id: (*id).into(),
                            outcome: ToolOutcome::Success { value: json!(1) },
                        })
                        .collect(),
                },
            )
            .unwrap();
        let Action::CallModel { request } = next.action else {
            panic!("expected model")
        };
        assert_eq!(
            encode(&request, 100).unwrap()["messages"][1]["content"],
            original
        );
    }
}
