use hudson_core::{
    adapters::{
        models::{ModelExecutor, ModelObserver, RuntimeCallContext, RuntimeHistoryMode},
        tools::{ExecutionError, ToolRegistry},
    },
    models::*,
    runtime::Runtime,
    storage::Store,
};
use hudson_harness::{AgentLoop, ModelRequest, ModelResponse};
use serde_json::json;
use std::sync::{Arc, Mutex};
struct Model(Arc<Mutex<Vec<RuntimeCallContext>>>);
impl ModelExecutor for Model {
    fn call(&mut self, _: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        panic!("context must use admitted observation")
    }
    fn call_observed(
        &mut self,
        request: &ModelRequest,
        observer: &mut dyn ModelObserver,
    ) -> Result<ModelResponse, ExecutionError> {
        let context = observer
            .runtime_context(request)?
            .expect("admitted context");
        let mut calls = self.0.lock().unwrap();
        calls.push(context);
        Ok(ModelResponse::Final {
            output: json!(if calls.len() == 1 { 41 } else { 42 }),
        })
    }
}
#[test]
fn native_context_is_attempt_fenced_and_continues_same_owner_checkpoints() {
    let actor = Actor {
        workspace_id: "native-context".into(),
        id: "owner".into(),
    };
    let store = Store::default();
    let reference = VersionRef {
        id: "native".into(),
        version: 1,
    };
    store
        .publish_agent(Agent {
            id: reference.id.clone(),
            version: 1,
            workspace_id: actor.workspace_id.clone(),
            schema_version: SCHEMA_VERSION,
            name: "native".into(),
            instructions: "return 42".into(),
            model: "fixture".into(),
            allow_user_input: false,
            tools: vec![],
            input_schema: None,
            output_schema: Some(json!({"const":42})),
            limits: Limits::default(),
            created_at: 0,
        })
        .unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = Runtime::new(
        store.clone(),
        AgentLoop,
        Model(contexts.clone()),
        ToolRegistry::new(),
    );
    let first = runtime
        .submit(&actor, reference.clone(), json!("first"), None)
        .unwrap();
    for _ in 0..16 {
        if runtime.tick(&actor, first).unwrap().status.terminal() {
            break;
        }
    }
    assert_eq!(
        store.inspect(&actor, first).unwrap().status,
        RunStatus::Completed
    );
    {
        let calls = contexts.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].actor, actor);
        assert!(calls[0].previous.is_none());
        assert_eq!(calls[1].history_mode, RuntimeHistoryMode::Append);
        let previous = calls[1].previous.as_ref().unwrap();
        assert_eq!(previous.operation_id, calls[0].call.operation_id);
        assert_eq!(previous.attempt_id, calls[0].call.attempt_id);
        assert_eq!(previous.request_fingerprint, calls[0].request_fingerprint);
    }
    let next = runtime
        .submit_turn(&actor, reference, json!("follow-up"), None, first)
        .unwrap();
    for _ in 0..8 {
        if runtime.tick(&actor, next).unwrap().status.terminal() {
            break;
        }
    }
    let calls = contexts.lock().unwrap();
    assert_eq!(calls.len(), 3);
    let previous = calls[2].previous.as_ref().unwrap();
    assert_eq!(previous.run_id, first);
    assert_eq!(previous.operation_id, calls[1].call.operation_id);
    assert_eq!(calls[2].call.run_id, next);
}
