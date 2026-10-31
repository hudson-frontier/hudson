//! Offline SDK example: cargo run --locked -p hudson-core --example embedded
use hudson_core::{
    adapters::{models::ModelExecutor, tools::ExecutionError},
    models::*,
    sdk::SessionBuilder,
    security::Policy,
};
use hudson_harness::{Content, ModelRequest, ModelResponse, ToolCall, ToolOutcome};
use serde_json::{json, Value};

struct Arithmetic;
impl ModelExecutor for Arithmetic {
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        for message in request.messages.iter().rev() {
            for content in &message.content {
                if let Content::ToolResult { result } = content {
                    if let ToolOutcome::Success { value } = &result.outcome {
                        return Ok(ModelResponse::Final {
                            output: value.clone(),
                        });
                    }
                }
            }
        }
        Ok(ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                call_id: "sum-1".into(),
                name: "sum".into(),
                arguments: json!({"a":17,"b":25}),
                provider_metadata: Value::Null,
            }],
        })
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let actor = Actor {
        workspace_id: "example".into(),
        id: "developer".into(),
    };
    let mut builder = SessionBuilder::new(
        actor.clone(),
        "assistant",
        "Use sum and return its result.",
        "offline-arithmetic",
        Arithmetic,
    );
    builder.register_tool(Tool {
        id: "sum".into(), workspace_id: actor.workspace_id.clone(), version: 1,
        schema_version: SCHEMA_VERSION, name: "sum".into(), description: "Add two integers.".into(),
        input_schema: json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"],"additionalProperties":false}),
        output_schema: None, execution: Execution::Registered { key: "sum".into() },
        credential_ref: None, policy_ref: "sum-policy".into(), effect: Effect::Read, created_at: 0,
    }, Policy { actors: [actor.id].into(), ..Policy::default() }, |call| {
        let a = call.arguments["a"].as_i64().ok_or_else(|| ExecutionError::Failed("integer a required".into()))?;
        let b = call.arguments["b"].as_i64().ok_or_else(|| ExecutionError::Failed("integer b required".into()))?;
        let sum = a.checked_add(b).ok_or_else(|| ExecutionError::Failed("sum overflow".into()))?;
        Ok(json!({"sum":sum}))
    })?;
    let mut session = builder.build()?;
    let run = session.submit(json!("Add 17 and 25"), "example-1".into(), Some(Goal {
        objective: "Return the sum".into(), success_schema: json!({"type":"object","properties":{"sum":{"const":42}},"required":["sum"]}), criteria: vec![],
    }))?;
    for _ in 0..128 {
        let view = session.advance(run)?;
        if view.status.terminal()
            || matches!(view.status, RunStatus::Waiting | RunStatus::Cancelling)
        {
            println!("{}", serde_json::to_string_pretty(&view)?);
            if view.status != RunStatus::Completed {
                return Err("example did not complete".into());
            }
            return Ok(());
        }
    }
    Err("host tick budget exhausted".into())
}
