use hudson_core::{
    adapters::{
        models::{ModelExecutor, ModelObserver},
        tools::{ExecutionError, ToolRegistry},
    },
    budgets::BudgetedModel,
    models::*,
    runtime::Runtime,
    storage::Store,
};
use hudson_harness::{AgentLoop, ModelRequest, ModelResponse};
use serde_json::json;

struct Streaming {
    chunks: Vec<String>,
    fail: bool,
}
impl ModelExecutor for Streaming {
    fn call(&mut self, _: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        Err(ExecutionError::Failed(
            "expected observed entry point".into(),
        ))
    }
    fn call_observed(
        &mut self,
        _: &ModelRequest,
        observer: &mut dyn ModelObserver,
    ) -> Result<ModelResponse, ExecutionError> {
        assert!(observer.context().is_some());
        for text in &self.chunks {
            observer.text_delta(text)?;
        }
        if self.fail {
            return Err(ExecutionError::Unknown(
                "fixture response interrupted".into(),
            ));
        }
        Ok(ModelResponse::Final { output: json!(42) })
    }
}
fn actor() -> Actor {
    Actor {
        workspace_id: "stream-tests".into(),
        id: "owner".into(),
    }
}
fn reference() -> VersionRef {
    VersionRef {
        id: "streaming".into(),
        version: 1,
    }
}
fn make_runtime(
    chunks: Vec<String>,
    fail: bool,
    limit: usize,
) -> Runtime<AgentLoop, BudgetedModel<Streaming>, ToolRegistry> {
    let store = Store::default();
    store
        .ensure_agent(Agent {
            id: "streaming".into(),
            workspace_id: actor().workspace_id,
            version: 1,
            schema_version: SCHEMA_VERSION,
            name: "streaming".into(),
            allow_user_input: false,
            instructions: "help".into(),
            model: "fixture".into(),
            tools: vec![],
            input_schema: None,
            output_schema: None,
            limits: Limits {
                max_payload_bytes: limit,
                ..Limits::default()
            },
            created_at: 0,
        })
        .unwrap();
    let model = BudgetedModel::new(
        Streaming { chunks, fail },
        store.clone(),
        "stream-tests",
        "batch",
        1,
    )
    .unwrap();
    Runtime::new(store, AgentLoop, model, ToolRegistry::new())
}
#[test]
fn provisional_output_is_buffered_scoped_and_fenced_to_the_admitted_attempt() {
    let mut runtime = make_runtime((0..2000).map(|_| "héllo🌍".into()).collect(), false, 65536);
    let run = runtime
        .submit(&actor(), reference(), json!("answer"), Some("one".into()))
        .unwrap();
    for _ in 0..8 {
        if runtime.tick(&actor(), run).unwrap().status.terminal() {
            break;
        }
    }
    assert_eq!(
        runtime.store.inspect(&actor(), run).unwrap().result,
        Some(json!(42))
    );
    let operations = runtime.store.operations(&actor(), run).unwrap();
    let model = operations
        .iter()
        .find(|op| matches!(op.request, OperationRequest::Model { .. }))
        .unwrap();
    let events = runtime.store.event_page(&actor(), run, 0, 1000).unwrap();
    let deltas = events
        .iter()
        .filter(|e| e.event_type == EventType::ModelOutputDelta)
        .collect::<Vec<_>>();
    assert!(
        deltas.len() < 16,
        "must batch tokens rather than rewrite storage for every delta"
    );
    let text = deltas
        .iter()
        .map(|e| {
            assert_eq!(e.operation_id, Some(model.meta.id));
            assert_eq!(e.payload["attempt_id"], json!(model.attempts[0].id));
            assert_eq!(e.payload["provisional"], true);
            let text = e.payload["text"].as_str().unwrap();
            assert!(text.len() <= 4096);
            text
        })
        .collect::<String>();
    assert_eq!(text, "héllo🌍".repeat(2000));
    assert_eq!(runtime.model_budget_binding().unwrap().group, "batch");
    assert_eq!(
        runtime
            .store
            .inspect(&actor(), run)
            .unwrap()
            .usage
            .model_calls,
        1
    );
    let mut other = actor();
    other.id = "another-actor".into();
    assert!(runtime.store.event_page(&other, run, 0, 1000).is_err());
    let count = events.len();
    runtime.tick(&actor(), run).unwrap();
    assert_eq!(
        runtime
            .store
            .event_page(&actor(), run, 0, 1000)
            .unwrap()
            .len(),
        count
    );
}
#[test]
fn oversized_or_interrupted_streams_cannot_become_successful_final_results() {
    for (chunks, fail) in [
        (vec!["x".repeat(1025)], false),
        (vec!["partial text".into()], true),
    ] {
        let mut runtime = make_runtime(chunks, fail, 1024);
        let run = runtime
            .submit(&actor(), reference(), json!("answer"), None)
            .unwrap();
        for _ in 0..6 {
            let view = runtime.tick(&actor(), run).unwrap();
            if view.status.terminal() || view.wait.is_some() {
                break;
            }
        }
        let view = runtime.store.inspect(&actor(), run).unwrap();
        assert_ne!(view.status, RunStatus::Completed);
        assert!(view.result.is_none());
        assert_eq!(view.usage.model_calls, 1);
        assert_eq!(runtime.store.operations(&actor(), run).unwrap().len(), 1);
    }
}
