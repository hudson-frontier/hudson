//! Text/function-calling adapter for the OpenAI Chat Completions protocol.
use super::{models::ModelExecutor, tools::ExecutionError};
use hudson_harness::{Content, ModelRequest, ModelResponse, Role, ToolCall};
use serde_json::{json, Value};
use std::{io::Read, time::Duration};

pub struct ChatModel {
    client: reqwest::blocking::Client,
    endpoint: reqwest::Url,
    api_key: Option<String>,
    max_output_tokens: u32,
    legacy_token_limit: bool,
    last_usage: Option<crate::models::TokenUsage>,
    reasoning: Option<super::reasoning::Reasoning>,
}
fn failed(message: &str) -> ExecutionError {
    ExecutionError::Failed(message.into())
}

impl ChatModel {
    pub fn with_reasoning(
        mut self,
        reasoning: Option<super::reasoning::Reasoning>,
    ) -> crate::Result<Self> {
        if let Some(reasoning) = &reasoning {
            reasoning.validate("openai", self.max_output_tokens)?;
        }
        self.reasoning = reasoning;
        Ok(self)
    }
    /// Compatibility mode for servers accepting max_tokens instead of max_completion_tokens.
    pub fn with_legacy_token_limit(mut self, enabled: bool) -> Self {
        self.legacy_token_limit = enabled;
        self
    }
    /// Endpoint is the full `/v1/chat/completions` URL; credentials stay outside checkpoints.
    pub fn new(
        endpoint: &str,
        api_key: Option<String>,
        max_output_tokens: u32,
    ) -> crate::Result<Self> {
        Self::build(endpoint, api_key, max_output_tokens, false)
    }

    pub(crate) fn fixture(endpoint: &str, max_output_tokens: u32) -> crate::Result<Self> {
        Self::build(endpoint, None, max_output_tokens, true)
    }

    fn build(
        endpoint: &str,
        api_key: Option<String>,
        max_output_tokens: u32,
        no_proxy: bool,
    ) -> crate::Result<Self> {
        let endpoint = reqwest::Url::parse(endpoint)
            .map_err(|_| crate::Error::Invalid("invalid model endpoint".into()))?;
        let local = matches!(
            endpoint.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]")
        );
        if (endpoint.scheme() != "https" && !(endpoint.scheme() == "http" && local))
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || (no_proxy && !matches!(endpoint.host_str(), Some("127.0.0.1" | "[::1]")))
            || max_output_tokens == 0
        {
            return Err(crate::Error::Invalid("model endpoint requires HTTPS (or local HTTP), no URL credentials, and a positive token limit".into()));
        }
        let builder = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none());
        let builder = if no_proxy || (endpoint.scheme() == "http" && local) {
            builder.no_proxy()
        } else {
            builder
        };
        let client = builder
            .build()
            .map_err(|_| crate::Error::Invalid("cannot construct model HTTP client".into()))?;
        Ok(Self {
            client,
            endpoint,
            api_key,
            max_output_tokens,
            legacy_token_limit: false,
            last_usage: None,
            reasoning: None,
        })
    }
}

impl ModelExecutor for ChatModel {
    fn take_usage(&mut self) -> Option<crate::models::TokenUsage> {
        self.last_usage.take()
    }
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        self.last_usage = None;
        let mut body = encode(request, self.max_output_tokens)?;
        if self.legacy_token_limit {
            let limit = body
                .as_object_mut()
                .expect("encoded object")
                .remove("max_completion_tokens")
                .expect("token limit");
            body["max_tokens"] = limit;
        }
        if let Some(reasoning) = &self.reasoning {
            reasoning.apply(&mut body, false);
        }
        decode(self.post_json(&body, false)?)
    }
}
impl ChatModel {
    pub(super) fn post_anthropic_stream(
        &mut self,
        body: &Value,
        observer: &mut dyn FnMut(&str),
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<Value, ExecutionError> {
        self.last_usage = None;
        let mut http = self
            .client
            .post(self.endpoint.clone())
            .header("anthropic-version", "2023-06-01")
            .json(body);
        if let Some(key) = &self.api_key {
            http = http.header("x-api-key", key);
        }
        let response = http.send().map_err(|_| {
            ExecutionError::Unknown(
                "model transport failed; request may have been processed".into(),
            )
        })?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let category = super::provider_errors::classify(status, response);
            return Err(failed(&format!("model HTTP status {status}: {category}")));
        }
        let value = super::anthropic_stream::read(response, observer, cancelled)?;
        self.last_usage = reported_usage(&value, true);
        Ok(value)
    }

    pub(super) fn post_json(
        &mut self,
        body: &Value,
        anthropic: bool,
    ) -> Result<Value, ExecutionError> {
        self.last_usage = None;
        let mut http = self.client.post(self.endpoint.clone()).json(&body);
        if let Some(key) = &self.api_key {
            http = if anthropic {
                http.header("x-api-key", key)
            } else {
                http.bearer_auth(key)
            };
        }
        if anthropic {
            http = http.header("anthropic-version", "2023-06-01");
        }
        let response = http.send().map_err(|_| {
            ExecutionError::Unknown(
                "model transport failed; request may have been processed".into(),
            )
        })?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            // Inspect only a bounded body and retain only fixed categories. Providers
            // can reflect prompts or credentials in any error field, including codes.
            let category = super::provider_errors::classify(status, response);
            return Err(failed(&format!("model HTTP status {status}: {category}")));
        }
        let mut bytes = Vec::new();
        response
            .take(2_097_153)
            .read_to_end(&mut bytes)
            .map_err(|_| failed("could not read model response"))?;
        if bytes.len() > 2_097_152 {
            return Err(failed("model response exceeds 2 MiB"));
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| failed("model response is not JSON"))?;
        self.last_usage = reported_usage(&value, anthropic);
        Ok(value)
    }
}

fn reported_usage(value: &Value, anthropic: bool) -> Option<crate::models::TokenUsage> {
    let usage = value.get("usage")?;
    let (input_tokens, output_tokens) = if anthropic {
        let cached = |key| match usage.get(key) {
            None => Some(0),
            Some(value) => value.as_u64(),
        };
        (
            usage
                .get("input_tokens")?
                .as_u64()?
                .checked_add(cached("cache_read_input_tokens")?)?
                .checked_add(cached("cache_creation_input_tokens")?)?,
            usage.get("output_tokens")?.as_u64()?,
        )
    } else {
        (
            usage.get("prompt_tokens")?.as_u64()?,
            usage.get("completion_tokens")?.as_u64()?,
        )
    };
    Some(crate::models::TokenUsage {
        input_tokens,
        output_tokens,
    })
}

fn encode(request: &ModelRequest, max_tokens: u32) -> Result<Value, ExecutionError> {
    let mut messages = vec![json!({"role":"system", "content":request.instructions})];
    for message in &request.messages {
        let mut text = Vec::new();
        let mut calls = Vec::new();
        let mut assistant_text_seen = false;
        for content in &message.content {
            match content {
                Content::Text { text: value } => text.push(value.clone()),
                Content::Json { value } => text.push(value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string())),
                Content::ToolCall { call } if message.role == Role::Assistant => {
                    let mut item=json!({"id":call.call_id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}});
                    if !call.provider_metadata.is_null() {
                        let metadata=call.provider_metadata.as_object().ok_or_else(||failed("invalid provider metadata"))?;
                        for (key,value) in metadata {
                            if key == "chat_assistant_text" {
                                if assistant_text_seen { return Err(failed("duplicate assistant continuation text")); }
                                text.push(value.as_str().ok_or_else(||failed("invalid assistant continuation text"))?.to_owned());
                                assistant_text_seen = true;
                                continue;
                            }
                            if !matches!(key.as_str(),"extra_content" | "thought_signature") { return Err(failed("unsupported provider metadata field")); }
                            item[key]=value.clone();
                        }
                    }
                    calls.push(item);
                }
                Content::ToolResult { result } if message.role == Role::Tool => messages.push(json!({"role":"tool","tool_call_id":result.call_id,"content":serde_json::to_string(&result.outcome).map_err(|_| failed("invalid tool result"))?})),
                _ => return Err(failed("unsupported content for text chat adapter")),
            }
        }
        if message.role == Role::Tool {
            continue;
        }
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => unreachable!(),
        };
        let mut item = json!({"role":role,"content":text.join("\n")});
        if !calls.is_empty() {
            item["tool_calls"] = json!(calls);
        }
        messages.push(item);
    }
    let mut body =
        json!({"model":request.model,"messages":messages,"max_completion_tokens":max_tokens});
    if !request.tools.is_empty() {
        body["tools"] = json!(request.tools.iter().map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}})).collect::<Vec<_>>());
    }
    Ok(body)
}

fn decode(value: Value) -> Result<ModelResponse, ExecutionError> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .filter(|c| c.len() == 1)
        .and_then(|c| c.first())
        .ok_or_else(|| failed("expected one model choice"))?;
    let message = &choice["message"];
    if !message["refusal"].is_null() {
        return Err(failed("model refused the request"));
    }
    match choice["finish_reason"].as_str() {
        Some("tool_calls") => {
            let assistant_text = match message.get("content") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) => Some(text),
                _ => return Err(failed("unsupported assistant continuation content")),
            };
            let items = message["tool_calls"]
                .as_array()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| failed("missing tool calls"))?;
            let mut calls = Vec::new();
            for item in items {
                let mut metadata = serde_json::Map::new();
                // Keep message-level context once, rather than copying it into every call.
                // This field is restored as assistant content, never sent as a tool argument.
                if calls.is_empty() {
                    if let Some(text) = assistant_text.filter(|text| !text.is_empty()) {
                        metadata.insert("chat_assistant_text".into(), json!(text));
                    }
                }
                for key in ["extra_content", "thought_signature"] {
                    if let Some(value) = item.get(key) {
                        metadata.insert(key.into(), value.clone());
                    }
                }
                let string = |v: &Value| {
                    v.as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| failed("invalid model tool call"))
                };
                if item["type"] != "function" {
                    return Err(failed("unsupported model tool type"));
                }
                calls.push(ToolCall {
                    provider_metadata: if metadata.is_empty() {
                        Value::Null
                    } else {
                        Value::Object(metadata)
                    },
                    call_id: string(&item["id"])?,
                    name: string(&item["function"]["name"])?,
                    arguments: serde_json::from_str(&string(&item["function"]["arguments"])?)
                        .map_err(|_| failed("invalid tool arguments JSON"))?,
                });
            }
            Ok(ModelResponse::ToolCalls { calls })
        }
        Some("stop") => {
            let text = message["content"]
                .as_str()
                .ok_or_else(|| failed("missing model output"))?;
            Ok(ModelResponse::Final {
                output: serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
            })
        }
        _ => Err(failed("model output was incomplete or unsupported")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_truncated_tool_arguments_and_partial_answers() {
        assert!(decode(
            json!({"choices":[{"finish_reason":"length","message":{"content":"partial"}}]})
        )
        .is_err());
        assert!(decode(json!({"choices":[{"finish_reason":"tool_calls","message":{"tool_calls":[{"id":"a","type":"function","function":{"name":"query","arguments":"{"}}]}}]})).is_err());
    }
    #[test]
    fn parses_tools_and_structured_final_output() {
        assert!(matches!(decode(json!({"choices":[{"finish_reason":"tool_calls","message":{"tool_calls":[{"id":"a","type":"function","function":{"name":"query","arguments":"{}"}}]}}]})).unwrap(), ModelResponse::ToolCalls { .. }));
        assert_eq!(decode(json!({"choices":[{"finish_reason":"stop","message":{"content":"{\"answer\":42}"}}]})).unwrap(), ModelResponse::Final { output:json!({"answer":42}) });
    }
    #[test]
    fn refused_or_unsupported_assistant_content_cannot_dispatch_tools() {
        let call = json!({"id":"a","type":"function","function":{"name":"query","arguments":"{}"}});
        for content in [
            json!(false),
            json!([{"type":"refusal","refusal":"declined"}]),
        ] {
            assert!(decode(json!({"choices":[{"finish_reason":"tool_calls","message":{"content":content,"tool_calls":[call]}}]})).is_err());
        }
        assert!(decode(json!({"choices":[{"finish_reason":"tool_calls","message":{"refusal":"declined","tool_calls":[call]}}]})).is_err());
    }
}

#[cfg(test)]
mod continuation_tests {
    use super::*;
    use hudson_harness::{Action, AgentLoop, Config, Engine, Input, ToolOutcome, ToolResult};
    #[test]
    fn opaque_tool_metadata_survives_checkpoint_and_next_request() {
        let opaque = json!({"google":{"thought_signature":"opaque-signed-value"}});
        let response=decode(json!({"choices":[{"finish_reason":"tool_calls","message":{"content":"I will calculate the total.","tool_calls":[{"id":"call-1","type":"function","function":{"name":"sum","arguments":"{\"a\":1}"},"extra_content":opaque},{"id":"call-2","type":"function","function":{"name":"sum","arguments":"{\"a\":2}"}}]}}]})).unwrap();
        let config = Config {
            allow_user_input: false,
            instructions: "help".into(),
            model: "compatible".into(),
            tools: vec![],
            max_context_bytes: 10000,
        };
        let engine = Engine::new(AgentLoop);
        let start = engine
            .advance(
                &config,
                &engine.initial_state(),
                Input::Start {
                    value: json!("task"),
                },
            )
            .unwrap();
        let tool = engine
            .advance(&config, &start.checkpoint, Input::Model { response })
            .unwrap();
        let checkpoint =
            serde_json::from_value(serde_json::to_value(tool.checkpoint).unwrap()).unwrap();
        let next = engine
            .advance(
                &config,
                &checkpoint,
                Input::Tools {
                    results: vec![
                        ToolResult {
                            call_id: "call-1".into(),
                            outcome: ToolOutcome::Success { value: json!(1) },
                        },
                        ToolResult {
                            call_id: "call-2".into(),
                            outcome: ToolOutcome::Success { value: json!(2) },
                        },
                    ],
                },
            )
            .unwrap();
        let Action::CallModel { request } = next.action else {
            panic!("expected model")
        };
        let body = encode(&request, 100).unwrap();
        let call = &body["messages"][2]["tool_calls"][0];
        assert_eq!(
            body["messages"][2]["content"],
            "I will calculate the total."
        );
        assert!(call.get("chat_assistant_text").is_none());
        assert_eq!(
            body["messages"][2]["tool_calls"].as_array().unwrap().len(),
            2
        );
        assert_eq!(call["extra_content"], opaque);
        assert_eq!(call["function"]["arguments"], "{\"a\":1}");
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    #[test]
    fn absent_partial_negative_or_overflowing_reports_remain_unavailable() {
        for usage in [
            json!(null),
            json!({"prompt_tokens":2}),
            json!({"prompt_tokens":-1,"completion_tokens":2}),
            json!({"prompt_tokens":1.5,"completion_tokens":2}),
        ] {
            assert!(reported_usage(&json!({"usage":usage}), false).is_none());
        }
        assert!(reported_usage(&json!({"usage":{"input_tokens":u64::MAX,"output_tokens":1,"cache_read_input_tokens":1}}),true).is_none());
        assert_eq!(
            reported_usage(
                &json!({"usage":{"prompt_tokens":0,"completion_tokens":0}}),
                false
            ),
            Some(crate::models::TokenUsage {
                input_tokens: 0,
                output_tokens: 0
            })
        );
    }
}
