#![cfg(feature = "fixtures")]
use hudson_core::{
    adapters::{models::ModelExecutor, tools::ExecutionError},
    conversations::{ImportedTurn, Transcript},
    fixtures::{self, FixtureTools},
    models::*,
    runtime::Runtime,
    security::Policy,
    storage::Store,
};
use hudson_harness::{
    AgentLoop, Content, Message, ModelRequest, ModelResponse, Role, ToolCall, ToolOutcome,
    ToolResult,
};
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
        Ok(self.responses.pop_front().expect("unexpected model call"))
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
    store.publish_agent(agent.clone()).unwrap();
    agent.version = 2;
    agent.model = "other-runtime-model".into();
    store.publish_agent(agent).unwrap();
}
fn cycle(store: Store) -> (uuid::Uuid, uuid::Uuid) {
    install(&store);
    let actor = fixtures::actor();
    let tools = FixtureTools::default();
    let effects = tools.executed.clone();
    let mut source = Runtime::new(
        store.clone(),
        AgentLoop,
        model(vec![
            ModelResponse::ToolCalls {
                calls: vec![ToolCall {
                    call_id: "lookup-1".into(),
                    name: "lookup_order".into(),
                    arguments: json!({"order_id":"123"}),
                    provider_metadata: json!({"opaque":"provider-private-continuation","chat_assistant_text":"visible source explanation",
                        "anthropic_prefix":[{"type":"thinking","thinking":"private hidden reasoning","signature":"opaque signature"},{"type":"text","text":"visible source prefix"}],
                        "anthropic_suffix":[{"type":"text","text":"visible source suffix"}]}),
                }],
            },
            ModelResponse::Final {
                output: json!("original answer"),
            },
        ]),
        tools.clone(),
    );
    let prior = source
        .submit(&actor, fixtures::agent_ref(), json!("original task"), None)
        .unwrap();
    assert!(store.export_transcript(&actor, prior).is_err());
    assert_eq!(
        fixtures::drive(&mut source, &actor, prior).unwrap().status,
        RunStatus::Completed
    );
    let source_operations = store.operations(&actor, prior).unwrap();
    let transcript = store.export_transcript(&actor, prior).unwrap();
    let encoded = serde_json::to_string(&transcript).unwrap();
    assert!(!encoded.contains("provider-private-continuation"));
    assert!(!encoded.contains("private hidden reasoning"));
    for text in [
        "visible source explanation",
        "visible source prefix",
        "visible source suffix",
    ] {
        assert!(encoded.contains(text));
    }
    assert!(encoded.contains("original answer"));
    let foreign = Actor {
        id: "another-actor".into(),
        ..actor.clone()
    };
    assert!(store.export_transcript(&foreign, prior).is_err());
    let model = model(vec![ModelResponse::Final {
        output: json!("fresh answer"),
    }]);
    let observed = model.observed.clone();
    let mut target = Runtime::new(store.clone(), AgentLoop, model, tools);
    let reference = VersionRef {
        version: 2,
        ..fixtures::agent_ref()
    };
    let submit = |transcript: Transcript| {
        target.import_turn(
            &actor,
            reference.clone(),
            ImportedTurn {
                transcript,
                input: json!("explain it"),
                request_key: "import-once".into(),
                goal: None,
                source_run: Some(prior),
            },
            None,
        )
    };
    let next = submit(transcript.clone()).unwrap();
    assert_eq!(submit(transcript.clone()).unwrap(), next);
    let mut changed = transcript.clone();
    changed.messages[0].content = vec![Content::Json {
        value: json!("changed source"),
    }];
    assert!(submit(changed).is_err());
    assert_eq!(
        fixtures::drive(&mut target, &actor, next).unwrap().status,
        RunStatus::Completed
    );
    assert_eq!(
        effects.lock().unwrap().len(),
        1,
        "history must never replay tools"
    );
    assert_eq!(store.operations(&actor, prior).unwrap(), source_operations);
    let target_operations = store.operations(&actor, next).unwrap();
    assert!(!target_operations
        .iter()
        .any(|op| matches!(op.request, OperationRequest::Tool { .. })));
    let view = store.inspect(&actor, next).unwrap();
    assert_eq!(view.usage.model_calls, 1);
    assert_eq!(view.usage.tool_calls, 0);
    assert!(view.assessment.unwrap().evidence.is_empty());
    let requests = observed.lock().unwrap();
    assert_eq!(requests[0].model, "other-runtime-model");
    let messages = serde_json::to_string(&requests[0].messages).unwrap();
    for text in ["original task", "original answer", "explain it"] {
        assert!(messages.contains(text));
    }
    (prior, next)
}
#[test]
fn explicit_transfer_changes_agent_without_replaying_effects_or_importing_authority() {
    cycle(Store::default());
}

#[test]
#[ignore = "requires dedicated PostgreSQL database on /tmp"]
fn postgres_conversation_transfer_retains_idempotency_and_source_history() {
    let database = std::env::var("HUDSON_TEST_DATABASE").expect("dedicated test database");
    let namespace = format!("conversation-{}", uuid::Uuid::new_v4());
    let (prior, next) = cycle(Store::postgres_local("/tmp", &database, &namespace).unwrap());
    let reopened = Store::postgres_local("/tmp", &database, &namespace).unwrap();
    let actor = fixtures::actor();
    assert_eq!(
        reopened.inspect(&actor, next).unwrap().status,
        RunStatus::Completed
    );
    assert!(
        serde_json::to_string(&reopened.export_transcript(&actor, prior).unwrap())
            .unwrap()
            .contains("original answer")
    );
    assert_eq!(
        reopened.events(&actor, next, 0).unwrap()[0].payload["source_run"],
        json!(prior)
    );
}

#[test]
fn imports_reject_incomplete_exchanges_provider_state_unsupported_artifacts_and_limits() {
    let base = Transcript {
        format_version: 1,
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "first".into(),
            }],
        }],
        archived_exchanges: vec![],
    };
    base.validate(65536).unwrap();
    let call = ToolCall {
        call_id: "call-1".into(),
        name: "lookup".into(),
        arguments: json!({}),
        provider_metadata: serde_json::Value::Null,
    };
    let mut incomplete = base.clone();
    incomplete.messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::ToolCall { call: call.clone() }],
    });
    assert!(incomplete.validate(65536).is_err());
    incomplete.messages.push(Message {
        role: Role::Tool,
        content: vec![Content::ToolResult {
            result: ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::Success { value: json!(42) },
            },
        }],
    });
    incomplete.validate(65536).unwrap();
    if let Content::ToolCall { call } = &mut incomplete.messages[1].content[0] {
        call.provider_metadata = json!({"threadId":"not-portable"});
    }
    assert!(incomplete.validate(65536).is_err());
    let mut artifact = base.clone();
    artifact.messages[0].content = vec![Content::Artifact {
        id: "foreign-artifact".into(),
        media_type: "image/png".into(),
    }];
    assert!(artifact.validate(65536).is_err());
    assert!(base.validate(1).is_err());
    let mut version = base;
    version.format_version = 2;
    assert!(version.validate(65536).is_err());
}
