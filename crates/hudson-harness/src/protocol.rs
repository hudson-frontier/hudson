use crate::Checkpoint;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text { text: String },
    Json { value: Value },
    Image { image: crate::images::Image },
    Artifact { id: String, media_type: String },
    ToolCall { call: ToolCall },
    ToolResult { result: ToolResult },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    /// Opaque provider continuation fields; never passed as tool arguments.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub provider_metadata: Value,
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolOutcome {
    Success { value: Value },
    Error { code: String, message: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub outcome: ToolOutcome,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelRequest {
    pub instructions: String,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<crate::ToolDescriptor>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelResponse {
    ToolCalls { calls: Vec<ToolCall> },
    Final { output: Value },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    /// Host-owned messages applied after this boundary's input is finalized.
    Queued {
        input: Box<Input>,
        messages: Vec<Value>,
    },
    Start {
        value: Value,
    },
    StartImages {
        value: Value,
        images: Vec<crate::images::Image>,
    },
    Model {
        response: ModelResponse,
    },
    Tools {
        results: Vec<ToolResult>,
    },
    Verification {
        passed: bool,
        feedback: String,
    },
    User {
        value: Value,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    CallModel { request: ModelRequest },
    ExecuteTools { calls: Vec<ToolCall> },
    Verify { candidate: Value },
    WaitForInput { prompt: String },
    Complete { output: Value },
    Fail { reason: String },
}

/// One boundary at a time; ExecuteTools may contain a bounded batch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Transition {
    pub checkpoint: Checkpoint,
    pub action: Action,
}
