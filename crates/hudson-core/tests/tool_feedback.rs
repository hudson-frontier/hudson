#![cfg(feature = "fixtures")]
use hudson_core::{
    adapters::{models::ModelExecutor, tools::ExecutionError},
    fixtures::{self, FixtureTools},
    models::*,
    runtime::Runtime,
    security::Policy,
    storage::Store,
};
use hudson_harness::{AgentLoop, Content, ModelRequest, ModelResponse, ToolCall, ToolOutcome};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct Script {
    responses: VecDeque<ModelResponse>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}
impl ModelExecutor for Script {
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(self.responses.pop_front().expect("unexpected model call"))
    }
}
fn script(responses: Vec<ModelResponse>) -> Script {
    Script {
        responses: responses.into(),
        requests: Default::default(),
    }
}
fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        name: name.into(),
        arguments,
        provider_metadata: Value::Null,
    }
}
fn batch(calls: Vec<ToolCall>) -> ModelResponse {
    ModelResponse::ToolCalls { calls }
}
fn answer() -> ModelResponse {
    ModelResponse::Final {
        output: json!({"message":"done"}),
    }
}
fn install(store: &Store, max_calls: u32) {
    let (mut agent, tools) = fixtures::definitions();
    agent.limits.max_model_calls = max_calls;
    for tool in tools {
        store
            .set_policy(
                "demo",
                &tool.policy_ref,
                Policy {
                    actors: ["developer".into()].into(),
                    approvers: ["developer".into()].into(),
                    require_approval: tool.effect == Effect::Write,
                },
            )
            .unwrap();
        store.publish_tool(tool).unwrap();
    }
    store.publish_agent(agent).unwrap();
}
fn submit(runtime: &Runtime<AgentLoop, Script, FixtureTools>) -> uuid::Uuid {
    runtime
        .submit(
            &fixtures::actor(),
            fixtures::agent_ref(),
            json!({"order_id":"123","action":"lookup"}),
            None,
        )
        .unwrap()
}

fn repair_across_reopen(connect: impl Fn() -> Store) {
    let store = connect();
    install(&store, 3);
    let model = script(vec![batch(vec![call(
        "bad",
        "refund_order",
        json!({"order_id":"123","amount_cents":"private-invalid-value"}),
    )])]);
    let tools = FixtureTools::default();
    let effects = tools.executed.clone();
    let mut runtime = Runtime::new(store, AgentLoop, model, tools.clone());
    let actor = fixtures::actor();
    let id = submit(&runtime);
    // Stop with the invalid request durably pending; validate it after reopening.
    for _ in 0..3 {
        runtime.tick(&actor, id).unwrap();
    }
    assert!(effects.lock().unwrap().is_empty());
    drop(runtime);
    let model = script(vec![
        batch(vec![call(
            "corrected",
            "refund_order",
            json!({"order_id":"123","amount_cents":100}),
        )]),
        answer(),
    ]);
    let requests = model.requests.clone();
    let mut runtime = Runtime::new(connect(), AgentLoop, model, tools);
    let waiting = fixtures::drive(&mut runtime, &actor, id).unwrap();
    let Some(WaitReason::Approval { operation_id }) = waiting.wait else {
        panic!("corrected write needs its own approval")
    };
    assert!(effects.lock().unwrap().is_empty());
    let operations = runtime.store.operations(&actor, id).unwrap();
    let invalid = operations.iter().find(|op| matches!(&op.request, OperationRequest::Tool { call, .. } if call.call_id == "bad")).unwrap();
    assert_eq!(invalid.status, OperationStatus::Failed);
    assert!(invalid.attempts.is_empty());
    assert!(invalid.approval.is_none());
    let Some(OperationResult::Tool { result }) = &invalid.result else {
        panic!("correlated error required")
    };
    assert_eq!(result.call_id, "bad");
    assert!(
        matches!(&result.outcome, ToolOutcome::Error { code, message } if code == "invalid_arguments" && !message.contains("private-invalid-value"))
    );
    assert!(requests.lock().unwrap()[0].messages.iter().flat_map(|m| &m.content).any(|content|
        matches!(content, Content::ToolResult { result } if result.call_id == "bad" && matches!(&result.outcome, ToolOutcome::Error { code, .. } if code == "invalid_arguments"))));
    runtime
        .approve(&actor, operation_id, true, now(), now() + 60_000)
        .unwrap();
    let completed = fixtures::drive(&mut runtime, &actor, id).unwrap();
    assert_eq!(completed.status, RunStatus::Completed);
    assert_eq!(completed.usage.model_calls, 3);
    assert_eq!(completed.usage.tool_calls, 2);
    assert_eq!(completed.usage.operations, 6);
    assert_eq!(effects.lock().unwrap().as_slice(), &[operation_id]);
    // A terminal tick and a second reopen cannot repeat either tool call.
    runtime.tick(&actor, id).unwrap();
    drop(runtime);
    let mut runtime = Runtime::new(
        connect(),
        AgentLoop,
        script(vec![]),
        FixtureTools::default(),
    );
    assert_eq!(
        runtime.tick(&actor, id).unwrap().status,
        RunStatus::Completed
    );
}

#[test]
fn invalid_write_is_corrected_after_reopen_and_still_requires_approval() {
    let store = Store::default();
    repair_across_reopen(|| store.clone());
}

#[test]
#[ignore = "requires HUDSON_TEST_DATABASE in local PostgreSQL"]
fn invalid_argument_feedback_and_correction_survive_postgres_reconnect() {
    let database = std::env::var("HUDSON_TEST_DATABASE").expect("isolated test database");
    let namespace = format!("tool-feedback-{}", uuid::Uuid::new_v4());
    repair_across_reopen(|| Store::postgres_local("/tmp", &database, &namespace).unwrap());
}

#[test]
fn invalid_calls_consume_the_ordinary_budget_without_any_tool_effect() {
    let store = Store::default();
    install(&store, 2);
    let model = script(vec![
        batch(vec![call("bad-1", "lookup_order", json!({}))]),
        batch(vec![call("bad-2", "lookup_order", json!({}))]),
    ]);
    let requests = model.requests.clone();
    let tools = FixtureTools::default();
    let effects = tools.executed.clone();
    let mut runtime = Runtime::new(store, AgentLoop, model, tools);
    let id = submit(&runtime);
    let view = fixtures::drive(&mut runtime, &fixtures::actor(), id).unwrap();
    assert_eq!(view.status, RunStatus::Failed);
    assert_eq!(view.usage.model_calls, 2);
    assert_eq!(view.usage.tool_calls, 2);
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(effects.lock().unwrap().is_empty());
}

#[test]
fn ordered_batch_retains_successes_and_correlates_invalid_arguments() {
    let store = Store::default();
    install(&store, 2);
    let model = script(vec![
        batch(vec![
            call("first", "lookup_order", json!({"order_id":"123"})),
            call("invalid", "lookup_order", json!({})),
            call("last", "lookup_order", json!({"order_id":"456"})),
        ]),
        answer(),
    ]);
    let requests = model.requests.clone();
    let tools = FixtureTools::default();
    let effects = tools.executed.clone();
    let mut runtime = Runtime::new(store, AgentLoop, model, tools);
    let id = submit(&runtime);
    assert_eq!(
        fixtures::drive(&mut runtime, &fixtures::actor(), id)
            .unwrap()
            .status,
        RunStatus::Completed
    );
    let requests = requests.lock().unwrap();
    let results: Vec<_> = requests[1]
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResult { result } => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(
        results
            .iter()
            .map(|r| r.call_id.as_str())
            .collect::<Vec<_>>(),
        ["first", "invalid", "last"]
    );
    assert!(matches!(results[0].outcome, ToolOutcome::Success { .. }));
    assert!(
        matches!(&results[1].outcome, ToolOutcome::Error { code, .. } if code == "invalid_arguments")
    );
    assert!(matches!(results[2].outcome, ToolOutcome::Success { .. }));
    assert_eq!(effects.lock().unwrap().len(), 2);
}

#[test]
fn denial_and_cancellation_take_precedence_over_invalid_argument_feedback() {
    for cancel in [false, true] {
        let store = Store::default();
        install(&store, 2);
        store
            .set_policy("demo", "lookup_order", Policy::default())
            .unwrap();
        let model = script(vec![
            batch(vec![call("bad", "lookup_order", json!({}))]),
            answer(),
        ]);
        let tools = FixtureTools::default();
        let effects = tools.executed.clone();
        let mut runtime = Runtime::new(store, AgentLoop, model, tools);
        let actor = fixtures::actor();
        let id = submit(&runtime);
        for _ in 0..3 {
            runtime.tick(&actor, id).unwrap();
        }
        if cancel {
            runtime.cancel(&actor, id).unwrap();
        }
        let view = fixtures::drive(&mut runtime, &actor, id).unwrap();
        assert_eq!(
            view.status,
            if cancel {
                RunStatus::Cancelled
            } else {
                RunStatus::Completed
            }
        );
        assert!(effects.lock().unwrap().is_empty());
        let operations = runtime.store.operations(&actor, id).unwrap();
        let tool = operations
            .iter()
            .find(|op| matches!(op.request, OperationRequest::Tool { .. }))
            .unwrap();
        assert_eq!(
            tool.status,
            if cancel {
                OperationStatus::Cancelled
            } else {
                OperationStatus::Denied
            }
        );
        assert!(tool.attempts.is_empty());
    }
}

#[test]
fn uncertain_effect_blocks_later_feedback_and_malformed_identity_still_fails_closed() {
    for unknown in [false, true] {
        let store = Store::default();
        install(&store, 2);
        let calls = if unknown {
            vec![
                call("uncertain", "lookup_order", json!({"order_id":"123"})),
                call("bad", "lookup_order", json!({})),
            ]
        } else {
            vec![call("bad", "unregistered", json!({}))]
        };
        let tools = FixtureTools {
            uncertain: true,
            ..Default::default()
        };
        let effects = tools.executed.clone();
        let mut runtime = Runtime::new(store, AgentLoop, script(vec![batch(calls)]), tools);
        let actor = fixtures::actor();
        let id = submit(&runtime);
        let view = fixtures::drive(&mut runtime, &actor, id).unwrap();
        if unknown {
            assert!(matches!(view.wait, Some(WaitReason::Reconciliation { .. })));
            for _ in 0..3 {
                runtime.tick(&actor, id).unwrap();
            }
            let operations = runtime.store.operations(&actor, id).unwrap();
            let later = operations.iter().find(|op| matches!(&op.request, OperationRequest::Tool { call, .. } if call.call_id == "bad")).unwrap();
            assert_eq!(later.status, OperationStatus::Pending);
            assert!(later.result.is_none());
            assert_eq!(effects.lock().unwrap().len(), 1);
        } else {
            assert_eq!(view.status, RunStatus::Failed);
            assert!(effects.lock().unwrap().is_empty());
        }
    }
}
