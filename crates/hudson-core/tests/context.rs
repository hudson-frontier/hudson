#![cfg(feature = "fixtures")]
use hudson_core::{
    adapters::{
        models::ModelExecutor,
        tools::{ExecutionError, ToolRegistry},
    },
    context::{self, ContextPolicy},
    fixtures,
    models::*,
    runtime::Runtime,
    security::Policy,
    storage::Store,
};
use hudson_harness::{
    AgentLoop, Content, ModelRequest, ModelResponse, Role, ToolCall, ToolOutcome,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Observed {
    calls: usize,
    archived: bool,
    archive_id: Option<String>,
    artifact: Option<String>,
    source: String,
}
struct Reader(Arc<Mutex<Observed>>);
impl ModelExecutor for Reader {
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        assert!(serde_json::to_vec(request).unwrap().len() <= 6500);
        assert_eq!(
            request.messages[0].content,
            vec![Content::Json {
                value: json!("Find the original evidence; do not guess.")
            }]
        );
        let mut observed = self.0.lock().unwrap();
        observed.calls += 1;
        for message in &request.messages {
            for content in &message.content {
                match content {
                    Content::Text { text } if text.contains("Earlier conversation is archived") => {
                        observed.archived = true;
                        observed.archive_id = text
                            .split("using artifact_id ")
                            .nth(1)
                            .and_then(|tail| tail.split('.').next())
                            .map(str::to_owned);
                    }
                    Content::ToolCall { call } => assert_eq!(
                        call.provider_metadata,
                        json!({"thought_signature":"preserve-this"})
                    ),
                    _ => {}
                }
            }
        }
        // Every retained assistant batch has all corresponding tool results.
        for (index, message) in request.messages.iter().enumerate() {
            if message.role == Role::Assistant {
                for content in &message.content {
                    if let Content::ToolCall { call } = content {
                        assert!(request.messages[index+1..].iter().flat_map(|m| &m.content).any(|c| matches!(c, Content::ToolResult { result } if result.call_id == call.call_id)));
                    }
                }
            }
        }
        let result = request
            .messages
            .iter()
            .rev()
            .flat_map(|m| &m.content)
            .find_map(|c| match c {
                Content::ToolResult { result } => match &result.outcome {
                    ToolOutcome::Success { value } => Some(value),
                    _ => panic!("reader tool failed"),
                },
                _ => None,
            });
        let (name, arguments) = match result {
            None => ("lookup_order", json!({"order_id":"evidence"})),
            Some(value) if value.get("context_artifact").is_some() => {
                let id = value["context_artifact"]["artifact_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                observed.artifact = Some(id.clone());
                ("read_context_artifact", json!({"artifact_id":id}))
            }
            Some(value) => {
                observed.source.push_str(value["text"].as_str().unwrap());
                if let Some(offset) = value["next_offset"].as_u64() {
                    (
                        "read_context_artifact",
                        json!({"artifact_id":observed.artifact,"offset":offset}),
                    )
                } else {
                    let original: Value = serde_json::from_str(&observed.source).unwrap();
                    assert_eq!(original["value"]["answer"], "original-evidence-🦀");
                    return Ok(ModelResponse::Final {
                        output: json!({"answer":original["value"]["answer"]}),
                    });
                }
            }
        };
        Ok(ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                call_id: format!("call-{}", observed.calls),
                name: name.into(),
                arguments,
                provider_metadata: json!({"thought_signature":"preserve-this"}),
            }],
        })
    }
}
fn tools_registry(store: Store) -> (ToolRegistry, Tool, ContextPolicy) {
    let mut registry = ToolRegistry::new();
    registry
        .register("orders.lookup", |_| {
            Ok(json!({"answer":"original-evidence-🦀","data":"x".repeat(14000)}))
        })
        .unwrap();
    let policy = ContextPolicy {
        reserve_bytes: 0,
        offload_bytes: 4096,
        recent_exchanges: 1,
        excerpt_bytes: 256,
        page_bytes: 512,
    };
    let tool = context::register(&mut registry, store, "demo", "context-read", &policy).unwrap();
    (registry, tool, policy)
}
fn install(store: &Store) -> ToolRegistry {
    let (registry, reader, policy) = tools_registry(store.clone());
    let (mut agent, mut tools) = fixtures::definitions();
    agent.input_schema = None;
    agent.output_schema = None;
    agent.tools.truncate(1);
    agent.tools.push(AgentTool {
        tool_ref: reader.reference(),
        alias: reader.name.clone(),
    });
    agent.limits.max_context_bytes = 6500;
    agent.limits.max_model_calls = 100;
    agent.limits.max_harness_steps = 300;
    agent.limits.max_operations = 220;
    tools.truncate(1);
    tools.push(reader);
    for tool in tools {
        store
            .ensure_policy(
                "demo",
                &tool.policy_ref,
                Policy {
                    actors: ["developer".into()].into(),
                    approvers: Default::default(),
                    require_approval: false,
                },
            )
            .unwrap();
        store.publish_tool(tool).unwrap();
    }
    store.publish_agent(agent).unwrap();
    store
        .bind_context_policy("demo", &fixtures::agent_ref(), Some(&policy))
        .unwrap();
    assert!(store
        .bind_context_policy("demo", &fixtures::agent_ref(), None)
        .is_err());
    registry
}
fn exercise(connect: impl Fn() -> Store) {
    let store = connect();
    let registry = install(&store);
    let observed = Arc::new(Mutex::new(Observed::default()));
    let actor = fixtures::actor();
    let mut runtime = Runtime::new(store, AgentLoop, Reader(observed.clone()), registry);
    let id = runtime
        .submit(
            &actor,
            fixtures::agent_ref(),
            json!("Find the original evidence; do not guess."),
            None,
        )
        .unwrap();
    // Stop once the original tool result has been offloaded and persisted.
    for _ in 0..30 {
        runtime.tick(&actor, id).unwrap();
        if observed.lock().unwrap().artifact.is_some() {
            break;
        }
    }
    let artifact = observed
        .lock()
        .unwrap()
        .artifact
        .clone()
        .expect("large result was archived");
    drop(runtime);
    let store = connect();
    let page = store
        .read_context_artifact(&actor, id, &artifact, 0, 512)
        .unwrap();
    assert_eq!(page["offset"], 0);
    assert!(store
        .read_context_artifact(
            &Actor {
                workspace_id: "other".into(),
                ..actor.clone()
            },
            id,
            &artifact,
            0,
            512
        )
        .is_err());
    assert!(store
        .read_context_artifact(
            &Actor {
                id: "other".into(),
                ..actor.clone()
            },
            id,
            &artifact,
            0,
            512
        )
        .is_err());
    let (registry, _, _) = tools_registry(store.clone());
    let mut runtime = Runtime::new(store, AgentLoop, Reader(observed.clone()), registry);
    let other = runtime
        .submit(&actor, fixtures::agent_ref(), json!("another run"), None)
        .unwrap();
    assert!(runtime
        .store
        .read_context_artifact(&actor, other, &artifact, 0, 512)
        .is_err());
    for _ in 0..300 {
        let view = runtime.tick(&actor, id).unwrap();
        if view.status == RunStatus::Completed {
            break;
        }
        assert_ne!(view.status, RunStatus::Failed, "{view:?}");
    }
    let view = runtime.store.inspect(&actor, id).unwrap();
    assert_eq!(view.status, RunStatus::Completed);
    assert!(
        observed.lock().unwrap().archived,
        "history exceeded budget and was compacted"
    );
    // Every compacted exchange remains retrievable with its signed call and full result.
    let mut archive = observed.lock().unwrap().archive_id.clone();
    let mut archived_count = 0;
    while let Some(archive_id) = archive {
        let mut source = String::new();
        let mut offset = 0;
        loop {
            let page = runtime
                .store
                .read_context_artifact(&actor, id, &archive_id, offset, 512)
                .unwrap();
            source.push_str(page["text"].as_str().unwrap());
            match page["next_offset"].as_u64() {
                Some(next) => offset = next as usize,
                None => break,
            }
        }
        let original: Value = serde_json::from_str(&source).unwrap();
        let messages: Vec<hudson_harness::Message> =
            serde_json::from_value(original["messages"].clone()).unwrap();
        let Content::ToolCall { call } = &messages[0].content[0] else {
            panic!("missing archived call")
        };
        assert_eq!(
            call.provider_metadata,
            json!({"thought_signature":"preserve-this"})
        );
        let Content::ToolResult { result } = &messages[1].content[0] else {
            panic!("missing archived result")
        };
        assert_eq!(call.call_id, result.call_id);
        archive = original["previous_artifact"].as_str().map(str::to_owned);
        archived_count += 1;
        assert!(archived_count < 100);
    }
    assert!(archived_count > 1);
    // Source operations retain the full evidence as well as the immutable artifact.
    let ops = runtime.store.operations(&actor, id).unwrap();
    assert!(serde_json::to_string(&ops)
        .unwrap()
        .contains(&"x".repeat(14000)));
}
#[test]
fn long_run_retrieves_exact_evidence_with_complete_exchanges_and_scope_isolation() {
    let store = Store::default();
    exercise(|| store.clone());
}
#[test]
#[ignore = "requires HUDSON_TEST_DATABASE in local PostgreSQL"]
fn postgres_reconnect_recovers_context_and_original_artifacts() {
    let database = std::env::var("HUDSON_TEST_DATABASE").unwrap();
    let namespace = format!("context-{}", uuid::Uuid::new_v4());
    exercise(|| Store::postgres_local("/tmp", &database, &namespace).unwrap());
}
