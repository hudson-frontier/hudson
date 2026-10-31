use hudson_core::{
    adapters::{models::ModelExecutor, tools::ExecutionError},
    models::*,
    sdk::{Session, SessionBuilder},
    security::Policy,
    storage::Store,
};
use hudson_harness::{Content, ModelRequest, ModelResponse, ToolCall, ToolOutcome};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct Model {
    invalid: bool,
}
impl ModelExecutor for Model {
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        for message in &request.messages {
            for content in &message.content {
                if let Content::ToolResult { result } = content {
                    return Ok(ModelResponse::Final {
                        output: match &result.outcome {
                            ToolOutcome::Success { value } => value.clone(),
                            ToolOutcome::Error { code, .. } => json!({"error":code}),
                        },
                    });
                }
            }
        }
        Ok(ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                call_id: "call-1".into(),
                name: "echo".into(),
                arguments: if self.invalid {
                    json!({"wrong":true})
                } else {
                    json!({"value":42})
                },
                provider_metadata: Value::Null,
            }],
        })
    }
}
fn builder(
    store: Store,
    who: &str,
    approval: bool,
    invalid: bool,
    count: Arc<AtomicUsize>,
) -> SessionBuilder<Model> {
    let actor = Actor {
        workspace_id: "sdk-test".into(),
        id: who.into(),
    };
    let mut builder = SessionBuilder::new(
        actor.clone(),
        "assistant",
        "Echo using the tool.",
        "fixture",
        Model { invalid },
    )
    .with_store(store);
    builder.register_tool(Tool {
        id: "echo".into(), workspace_id: actor.workspace_id.clone(), version: 1, schema_version: SCHEMA_VERSION,
        name: "echo".into(), description: "Echo input".into(),
        input_schema: json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false}),
        output_schema: None, execution: Execution::Registered { key: "echo".into() }, credential_ref: None,
        policy_ref: "echo-policy".into(), effect: Effect::Write, created_at: 0,
    }, Policy { actors: ["owner".into()].into(), approvers: ["owner".into()].into(), require_approval: approval }, move |call| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(call.arguments.clone())
    }).unwrap();
    builder
}
fn drive(session: &mut Session<Model>, id: uuid::Uuid) -> RunView {
    for _ in 0..128 {
        let view = session.advance(id).unwrap();
        if view.status.terminal()
            || matches!(view.status, RunStatus::Waiting | RunStatus::Cancelling)
        {
            return view;
        }
    }
    panic!("fixture exceeded host tick budget")
}
#[test]
fn tool_cycle_is_idempotent_and_events_have_exclusive_cursors() {
    let count = Arc::new(AtomicUsize::new(0));
    let mut session = builder(Store::default(), "owner", false, false, count.clone())
        .build()
        .unwrap();
    let id = session.submit(json!("echo"), "task".into(), None).unwrap();
    assert_eq!(
        id,
        session.submit(json!("echo"), "task".into(), None).unwrap()
    );
    assert!(session
        .submit(json!("different"), "task".into(), None)
        .is_err());
    assert_eq!(drive(&mut session, id).result, Some(json!({"value":42})));
    assert_eq!(drive(&mut session, id).status, RunStatus::Completed);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let events = session.events(id, 0).unwrap();
    assert!(!events.is_empty());
    assert!(events
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence));
    let after = events.last().unwrap().sequence;
    assert!(session.events(id, after).unwrap().is_empty());
    let next = session
        .continue_turn(id, json!("continue"), "turn-2".into())
        .unwrap();
    assert_ne!(next, id);
    assert_eq!(drive(&mut session, next).status, RunStatus::Completed);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[test]
fn approval_and_reconstruction_preserve_pending_effects() {
    let store = Store::default();
    let count = Arc::new(AtomicUsize::new(0));
    let mut session = builder(store.clone(), "owner", true, false, count.clone())
        .build()
        .unwrap();
    let id = session.submit(json!("echo"), "task".into(), None).unwrap();
    let view = drive(&mut session, id);
    let Some(WaitReason::Approval { operation_id }) = view.wait else {
        panic!("expected approval");
    };
    assert_eq!(count.load(Ordering::SeqCst), 0);
    drop(session);
    let mut session = builder(store.clone(), "owner", true, false, count.clone())
        .build()
        .unwrap();
    session
        .approve(id, operation_id, true, now() + 60_000)
        .unwrap();
    assert_eq!(drive(&mut session, id).status, RunStatus::Completed);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    // Startup may not remove an existing approval gate.
    assert!(builder(store, "owner", false, false, count)
        .build()
        .is_err());
}
#[test]
fn invalid_arguments_never_reach_handler() {
    let count = Arc::new(AtomicUsize::new(0));
    let mut session = builder(Store::default(), "owner", false, true, count.clone())
        .build()
        .unwrap();
    let id = session.submit(json!("echo"), "task".into(), None).unwrap();
    assert_eq!(
        drive(&mut session, id).result,
        Some(json!({"error":"invalid_arguments"}))
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn controls_respect_actor_and_revision_and_cancellation() {
    let store = Store::default();
    let count = Arc::new(AtomicUsize::new(0));
    let session = builder(store.clone(), "owner", false, false, count.clone())
        .build()
        .unwrap();
    let id = session.submit(json!("echo"), "task".into(), None).unwrap();
    let receipt = session
        .steer(id, json!("Check carefully"), "direction")
        .unwrap();
    assert_eq!(
        receipt.id,
        session
            .steer(id, json!("Check carefully"), "direction")
            .unwrap()
            .id
    );
    assert!(session.steer(id, json!("different"), "direction").is_err());
    let stranger = builder(store.clone(), "stranger", false, false, count.clone())
        .build()
        .unwrap();
    assert!(stranger.inspect(id).is_err());
    assert!(stranger.events(id, 0).is_err());
    assert!(stranger.cancel(id).is_err());
    let mut other = builder(store, "owner", false, false, count.clone());
    other.definition_mut().version = 2;
    let other = other.build().unwrap();
    assert!(other.inspect(id).is_err());
    assert!(other.steer(id, json!("wrong revision"), "steer").is_err());
    session.cancel(id).unwrap();
    assert!(matches!(
        session.inspect(id).unwrap().status,
        RunStatus::Cancelled | RunStatus::Cancelling
    ));
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[cfg(unix)]
#[test]
fn local_snapshot_reopens_without_replaying_completed_tools() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("state.json");
    let count = Arc::new(AtomicUsize::new(0));
    let id = {
        let mut session = builder(
            Store::local_file(&path).unwrap(),
            "owner",
            false,
            false,
            count.clone(),
        )
        .build()
        .unwrap();
        let id = session.submit(json!("echo"), "task".into(), None).unwrap();
        assert_eq!(drive(&mut session, id).status, RunStatus::Completed);
        id
    };
    let mut session = builder(
        Store::local_file(&path).unwrap(),
        "owner",
        false,
        false,
        count.clone(),
    )
    .build()
    .unwrap();
    assert_eq!(
        session.submit(json!("echo"), "task".into(), None).unwrap(),
        id
    );
    assert_eq!(drive(&mut session, id).status, RunStatus::Completed);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn denied_policy_and_exhausted_limits_do_not_execute_tools() {
    let count = Arc::new(AtomicUsize::new(0));
    let mut denied = builder(Store::default(), "stranger", false, false, count.clone())
        .build()
        .unwrap();
    let id = denied.submit(json!("echo"), "denied".into(), None).unwrap();
    let view = drive(&mut denied, id);
    assert_eq!(view.result, Some(json!({"error":"denied"})));
    assert_eq!(count.load(Ordering::SeqCst), 0);

    let mut limited = builder(Store::default(), "owner", false, false, count.clone());
    limited.definition_mut().limits.max_operations = 1;
    let mut limited = limited.build().unwrap();
    let id = limited
        .submit(json!("echo"), "limited".into(), None)
        .unwrap();
    assert_eq!(drive(&mut limited, id).status, RunStatus::Failed);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
