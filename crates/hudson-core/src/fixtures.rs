//! Development fixtures only: scripted model, mock orders, in-memory storage.
//! No real provider, external business action, or production loop implementation.
use crate::adapters::{
    models::ModelExecutor,
    tools::{ExecutionError, Invocation, ToolExecutor},
};
use crate::{models::*, runtime::Runtime, security::Policy, storage::MemoryStore, Error, Result};
use hudson_harness::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub type DemoRuntime = Runtime<FixtureBackend, OrderModel, FixtureTools>;
pub fn actor() -> Actor {
    Actor {
        workspace_id: "demo".into(),
        id: "developer".into(),
    }
}
pub fn agent_ref() -> VersionRef {
    VersionRef {
        id: "order-assistant".into(),
        version: 1,
    }
}

#[derive(Default)]
pub struct FixtureBackend;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FixtureState {
    phase: String,
    messages: Vec<Message>,
    pending: Vec<ToolCall>,
    candidate: Option<Value>,
}

impl Backend for FixtureBackend {
    fn name(&self) -> &str {
        "fixture-only"
    }
    fn version(&self) -> u32 {
        1
    }
    fn advance(
        &self,
        config: &Config,
        checkpoint: &Checkpoint,
        input: Input,
    ) -> std::result::Result<Transition, HarnessError> {
        let mut state: FixtureState = if checkpoint.step == 0 {
            FixtureState::default()
        } else {
            serde_json::from_value(checkpoint.payload.clone())
                .map_err(|e| HarnessError::Invalid(e.to_string()))?
        };
        let action = match input {
            Input::Start { value } if state.phase.is_empty() => {
                state.messages.push(Message {
                    role: Role::User,
                    content: vec![Content::Json { value }],
                });
                state.phase = "model".into();
                Action::CallModel {
                    request: context::build(config, state.messages.clone())?,
                }
            }
            Input::Model { response } if state.phase == "model" => match response {
                ModelResponse::ToolCalls { calls } => {
                    state.messages.push(Message {
                        role: Role::Assistant,
                        content: calls
                            .iter()
                            .cloned()
                            .map(|call| Content::ToolCall { call })
                            .collect(),
                    });
                    state.pending = calls.clone();
                    state.phase = "tools".into();
                    Action::ExecuteTools { calls }
                }
                ModelResponse::Final { output } => {
                    state.candidate = Some(output.clone());
                    state.phase = "verify".into();
                    Action::Verify { candidate: output }
                }
            },
            Input::Tools { results } if state.phase == "tools" => {
                if results.len() != state.pending.len()
                    || results
                        .iter()
                        .zip(&state.pending)
                        .any(|(r, c)| r.call_id != c.call_id)
                {
                    return Err(HarnessError::Invalid(
                        "tool-result correlation failed".into(),
                    ));
                }
                state.messages.push(Message {
                    role: Role::Tool,
                    content: results
                        .into_iter()
                        .map(|result| Content::ToolResult { result })
                        .collect(),
                });
                state.pending.clear();
                state.phase = "model".into();
                Action::CallModel {
                    request: context::build(config, state.messages.clone())?,
                }
            }
            Input::Verification { passed, feedback } if state.phase == "verify" => {
                state.phase = "done".into();
                if passed {
                    Action::Complete {
                        output: state
                            .candidate
                            .clone()
                            .ok_or_else(|| HarnessError::Invalid("no candidate".into()))?,
                    }
                } else {
                    Action::Fail { reason: feedback }
                }
            }
            _ => return Err(HarnessError::Invalid("fixture input in wrong phase".into())),
        };
        Ok(Transition {
            checkpoint: Checkpoint {
                step: checkpoint.step + 1,
                payload: serde_json::to_value(state)
                    .map_err(|e| HarnessError::Invalid(e.to_string()))?,
                ..checkpoint.clone()
            },
            action,
        })
    }
}

/// Fixed order-demo behavior. It never contacts a language-model provider.
#[derive(Default)]
pub struct OrderModel;
impl ModelExecutor for OrderModel {
    fn call(
        &mut self,
        request: &ModelRequest,
    ) -> std::result::Result<ModelResponse, ExecutionError> {
        if let Some(result) = request
            .messages
            .iter()
            .rev()
            .flat_map(|m| &m.content)
            .find_map(|c| {
                if let Content::ToolResult { result } = c {
                    Some(result)
                } else {
                    None
                }
            })
        {
            let message = match &result.outcome {
                ToolOutcome::Success { value } => format!("Fixture result: {value}"),
                ToolOutcome::Error { message, .. } => {
                    format!("Could not perform the action: {message}")
                }
            };
            return Ok(ModelResponse::Final {
                output: json!({"message": message}),
            });
        }
        let value = request
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|c| {
                if let Content::Json { value } = c {
                    Some(value)
                } else {
                    None
                }
            })
            .ok_or_else(|| ExecutionError::Failed("missing fixture task".into()))?;
        let order_id = value["order_id"].as_str().unwrap_or("123");
        let refund = value["action"] == "refund";
        let call = ToolCall {
            provider_metadata: serde_json::Value::Null,
            call_id: "call_1".into(),
            name: if refund {
                "refund_order"
            } else {
                "lookup_order"
            }
            .into(),
            arguments: if refund {
                json!({"order_id": order_id, "amount_cents": 3000})
            } else {
                json!({"order_id": order_id})
            },
        };
        Ok(ModelResponse::ToolCalls { calls: vec![call] })
    }
}

#[derive(Clone, Default)]
pub struct FixtureTools {
    pub executed: Arc<Mutex<Vec<Uuid>>>,
    pub uncertain: bool,
}
impl ToolExecutor for FixtureTools {
    fn execute(
        &mut self,
        invocation: Invocation<'_>,
    ) -> std::result::Result<Value, ExecutionError> {
        self.executed
            .lock()
            .map_err(|_| ExecutionError::Failed("fixture mutex poisoned".into()))?
            .push(invocation.operation_id);
        if self.uncertain {
            return Err(ExecutionError::Unknown(
                "simulated lost acknowledgement".into(),
            ));
        }
        match &invocation.tool.execution {
            Execution::Registered { key } if key == "orders.lookup" => {
                Ok(json!({"status": "shipped"}))
            }
            Execution::Registered { key } if key == "orders.refund" => Ok(
                json!({"status": "refunded", "amount_cents": invocation.arguments["amount_cents"]}),
            ),
            _ => Err(ExecutionError::Failed("unknown fixture executor".into())),
        }
    }
}

pub fn definitions() -> (Agent, Vec<Tool>) {
    let tool = |id: &str, key: &str, effect: Effect, schema: Value| Tool {
        id: id.into(),
        workspace_id: "demo".into(),
        version: 1,
        schema_version: SCHEMA_VERSION,
        name: id.into(),
        description: format!("Simulated {id}; no real order system"),
        input_schema: schema,
        output_schema: None,
        execution: Execution::Registered { key: key.into() },
        credential_ref: None,
        policy_ref: id.into(),
        effect,
        created_at: now(),
    };
    let tools = vec![
        tool(
            "lookup_order",
            "orders.lookup",
            Effect::Read,
            json!({"type":"object","properties":{"order_id":{"type":"string","minLength":1}},
                "required":["order_id"],"additionalProperties":false}),
        ),
        tool(
            "refund_order",
            "orders.refund",
            Effect::Write,
            json!({"type":"object","properties":{"order_id":{"type":"string","minLength":1},
                "amount_cents":{"type":"integer","minimum":1,"maximum":3000}},
                "required":["order_id","amount_cents"],"additionalProperties":false}),
        ),
    ];
    let agent = Agent {
        id: agent_ref().id,
        workspace_id: "demo".into(),
        version: 1,
        schema_version: SCHEMA_VERSION,
        name: "Order fixture".into(),
        allow_user_input: false,
        instructions: "Use the simulated order tools, then report their result.".into(),
        model: "scripted-order-fixture".into(),
        tools: tools
            .iter()
            .map(|t| AgentTool {
                tool_ref: t.reference(),
                alias: t.name.clone(),
            })
            .collect(),
        input_schema: Some(json!({"type":"object","properties":{
            "order_id":{"type":"string","minLength":1},"action":{"enum":["lookup","refund"]}},
            "required":["order_id","action"],"additionalProperties":false})),
        output_schema: Some(
            json!({"type":"object","properties":{"message":{"type":"string"}},
            "required":["message"],"additionalProperties":false}),
        ),
        limits: Limits::default(),
        created_at: now(),
    };
    (agent, tools)
}

pub fn store() -> Result<MemoryStore> {
    let store = MemoryStore::default();
    let (agent, tools) = definitions();
    for tool in tools {
        store.set_policy(
            "demo",
            &tool.policy_ref,
            Policy {
                actors: ["developer".into()].into(),
                approvers: ["developer".into()].into(),
                require_approval: tool.effect == Effect::Write,
            },
        )?;
        store.publish_tool(tool)?;
    }
    store.publish_agent(agent)?;
    Ok(store)
}

pub fn runtime() -> Result<DemoRuntime> {
    Ok(Runtime::new(
        store()?,
        FixtureBackend,
        OrderModel,
        FixtureTools::default(),
    ))
}

pub fn drive<B: Backend, M: ModelExecutor, T: ToolExecutor>(
    runtime: &mut Runtime<B, M, T>,
    actor: &Actor,
    id: Uuid,
) -> Result<RunView> {
    for _ in 0..256 {
        let view = runtime.tick(actor, id)?;
        if view.status.terminal()
            || view.status == RunStatus::Waiting
            || view.status == RunStatus::Cancelling
        {
            return Ok(view);
        }
    }
    Err(Error::Invalid("fixture driver step ceiling reached".into()))
}

/// Fresh synthetic admitted Sandbox attempt for protocol tests only.
/// No real runtime admission, policy enforcement, VM, or existing Store is used.
/// This feature-gated helper cannot mutate caller-selected durable state.
pub fn synthetic_sandbox_protocol_fixture() -> Result<(
    crate::storage::Store,
    Actor,
    crate::sandbox_binding::Fence,
    crate::adapters::sandbox_recovery::PendingOperation,
)> {
    use crate::{
        adapters::sandbox_recovery::{PendingOperation, Step},
        definitions,
        sandbox_binding::Fence,
        storage::Store,
    };
    let store = Store::default();
    let mut runtime = runtime()?;
    let actor = actor();
    let run_id = runtime.submit(
        &actor,
        agent_ref(),
        json!({"order_id":"123","action":"lookup"}),
        None,
    )?;
    runtime.tick(&actor, run_id)?;
    let operation_id = runtime.store.operations(&actor, run_id)?[0].meta.id;
    let attempt_id = Uuid::new_v4();
    let mut snapshot = runtime.store.read(|d| Ok(d.clone()))?;
    let tool = snapshot.tools.values_mut().next().ok_or(Error::NotFound)?;
    tool.execution = Execution::Sandbox {
        package: "test-only".into(),
    };
    let tool_ref = tool.reference();
    let name = tool.name.clone();
    let op = snapshot
        .operations
        .get_mut(&operation_id)
        .ok_or(Error::NotFound)?;
    op.request = OperationRequest::Tool {
        tool_ref,
        call: hudson_harness::ToolCall {
            call_id: "fixture-call".into(),
            provider_metadata: serde_json::Value::Null,
            name,
            arguments: json!({}),
        },
    };
    op.request_digest =
        definitions::digest(&(operation_id, run_id, &actor.workspace_id, &op.request))?;
    snapshot
        .runs
        .get_mut(&run_id)
        .ok_or(Error::NotFound)?
        .pending_operations = vec![operation_id];
    op.status = OperationStatus::Running;
    op.attempts.push(Attempt {
        token_usage: None,
        id: attempt_id,
        number: 1,
        started_at: 1,
        finished_at: None,
        status: OperationStatus::Running,
        error: None,
    });
    // Storage protocol fixture: production Sandbox dispatch still fails closed.
    store.transact(|d| {
        *d = snapshot;
        Ok(())
    })?;
    let fence = Fence {
        run_id,
        operation_id,
        attempt_id,
    };
    let pending = PendingOperation {
        run_id: run_id.to_string(),
        workspace_id: actor.workspace_id.clone(),
        step: Step::Create,
        idempotency_key: "stable-create-key-001".into(),
        sandbox_id: None,
        operation_id: None,
        admission_attempted: false,
    };
    Ok((store, actor, fence, pending))
}
