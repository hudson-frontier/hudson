#![cfg(all(feature = "fixtures", unix))]
use hudson_core::{
    adapters::{models::ModelExecutor, tools::ExecutionError},
    fixtures::{self, FixtureTools},
    models::*,
    runtime::Runtime,
    security::Policy,
    storage::Store,
};
use hudson_harness::{AgentLoop, ModelRequest, ModelResponse, ToolCall};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct Script {
    responses: VecDeque<ModelResponse>,
    observed: Arc<Mutex<Vec<ModelRequest>>>,
}
impl ModelExecutor for Script {
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        self.observed.lock().unwrap().push(request.clone());
        Ok(self
            .responses
            .pop_front()
            .expect("unexpected repeated model call"))
    }
}
fn model(responses: Vec<ModelResponse>) -> Script {
    Script {
        responses: responses.into(),
        observed: Default::default(),
    }
}
fn install(store: &Store) {
    let (mut agent, tools) = fixtures::definitions();
    agent.input_schema = None;
    agent.output_schema = None;
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
fn lookup() -> ModelResponse {
    ModelResponse::ToolCalls {
        calls: vec![ToolCall {
            call_id: "lookup-once".into(),
            name: "lookup_order".into(),
            arguments: json!({"order_id":"123"}),
            provider_metadata: serde_json::Value::Null,
        }],
    }
}
fn private_directory() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

#[test]
fn follow_up_reopens_completed_history_without_replaying_previous_effects() {
    let directory = private_directory();
    let path = directory.path().join("state.json");
    let actor = fixtures::actor();
    let reference = fixtures::agent_ref();
    let store = Store::local_file(&path).unwrap();
    install(&store);
    let (mut other_version, _) = fixtures::definitions();
    other_version.input_schema = None;
    other_version.output_schema = None;
    other_version.version = 2;
    store.publish_agent(other_version).unwrap();
    let tools = FixtureTools::default();
    let effects = tools.executed.clone();
    let mut runtime = Runtime::new(
        store,
        AgentLoop,
        model(vec![
            lookup(),
            ModelResponse::Final {
                output: json!("saved original answer"),
            },
        ]),
        tools.clone(),
    );
    let prior = runtime
        .submit(&actor, reference.clone(), json!("original request"), None)
        .unwrap();
    assert!(runtime
        .submit_turn(&actor, reference.clone(), json!("too early"), None, prior)
        .is_err());
    assert_eq!(
        fixtures::drive(&mut runtime, &actor, prior).unwrap().status,
        RunStatus::Completed
    );
    let original_operations = runtime.store.operations(&actor, prior).unwrap();
    drop(runtime);

    let store = Store::local_file(&path).unwrap();
    let next_model = model(vec![ModelResponse::Final {
        output: json!("follow-up answer"),
    }]);
    let observed = next_model.observed.clone();
    let mut runtime = Runtime::new(store, AgentLoop, next_model, tools);
    assert!(runtime
        .submit_turn(
            &actor,
            VersionRef {
                version: 2,
                ..reference.clone()
            },
            json!("changed agent"),
            None,
            prior
        )
        .is_err());
    let foreign = Actor {
        id: "another actor".into(),
        ..actor.clone()
    };
    assert!(runtime.store.conversation_history(&foreign, prior).is_err());
    assert!(runtime
        .submit_turn(
            &foreign,
            reference.clone(),
            json!("steal history"),
            None,
            prior
        )
        .is_err());
    let key = Some("follow-up".into());
    let next = runtime
        .submit_turn(
            &actor,
            reference.clone(),
            json!("explain it"),
            key.clone(),
            prior,
        )
        .unwrap();
    assert_eq!(
        runtime
            .submit_turn(
                &actor,
                reference.clone(),
                json!("explain it"),
                key.clone(),
                prior
            )
            .unwrap(),
        next
    );
    assert!(runtime
        .submit_turn(&actor, reference, json!("changed"), key, prior)
        .is_err());
    assert_eq!(
        fixtures::drive(&mut runtime, &actor, next).unwrap().status,
        RunStatus::Completed
    );
    let messages = serde_json::to_string(&observed.lock().unwrap()[0].messages).unwrap();
    for text in ["original request", "saved original answer", "explain it"] {
        assert!(messages.contains(text));
    }
    assert_eq!(effects.lock().unwrap().len(), 1);
    assert_eq!(
        runtime.store.inspect(&actor, prior).unwrap().status,
        RunStatus::Completed
    );
    assert_eq!(
        runtime.store.operations(&actor, prior).unwrap(),
        original_operations
    );
}

#[test]
fn uncertain_effect_remains_recorded_without_replay_after_snapshot_reopen() {
    let directory = private_directory();
    let path = directory.path().join("state.json");
    let store = Store::local_file(&path).unwrap();
    install(&store);
    let tools = FixtureTools {
        uncertain: true,
        ..Default::default()
    };
    let effects = tools.executed.clone();
    let actor = fixtures::actor();
    let mut runtime = Runtime::new(store, AgentLoop, model(vec![lookup()]), tools.clone());
    let id = runtime
        .submit(&actor, fixtures::agent_ref(), json!("uncertain work"), None)
        .unwrap();
    assert_eq!(
        fixtures::drive(&mut runtime, &actor, id).unwrap().status,
        RunStatus::Waiting
    );
    let operations = runtime.store.operations(&actor, id).unwrap();
    assert!(operations
        .iter()
        .any(|operation| operation.status == OperationStatus::Unknown));
    drop(runtime);
    let store = Store::local_file(&path).unwrap();
    let mut reopened = Runtime::new(store, AgentLoop, model(vec![]), tools);
    assert_eq!(
        fixtures::drive(&mut reopened, &actor, id).unwrap().status,
        RunStatus::Waiting
    );
    assert_eq!(effects.lock().unwrap().len(), 1);
    assert_eq!(reopened.store.operations(&actor, id).unwrap(), operations);
}
