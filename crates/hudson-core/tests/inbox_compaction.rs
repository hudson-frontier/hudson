#![cfg(feature = "fixtures")]
use hudson_core::{
    adapters::{
        models::ModelExecutor,
        tools::{ExecutionError, ToolRegistry},
    },
    context::{self, ContextPolicy},
    fixtures,
    inbox::MessageKind,
    models::*,
    runtime::Runtime,
    security::Policy,
    storage::Store,
};
use hudson_harness::{AgentLoop, Content, ModelRequest, ModelResponse, ToolCall};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

struct Cycle(Arc<Mutex<Vec<ModelRequest>>>);
impl ModelExecutor for Cycle {
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        let mut requests = self.0.lock().unwrap();
        requests.push(request.clone());
        if request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::Json { value } if value == "new direction"))
        {
            return Ok(ModelResponse::Final {
                output: json!("answer"),
            });
        }
        Ok(ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                call_id: format!("lookup-{}", requests.len()),
                name: "lookup_order".into(),
                arguments: json!({"order_id":"evidence"}),
                provider_metadata: json!({"signature":"preserved"}),
            }],
        })
    }
}

fn registry(store: Store) -> (ToolRegistry, Tool, ContextPolicy) {
    let mut registry = ToolRegistry::new();
    registry
        .register("orders.lookup", |_| Ok(json!({"data":"x".repeat(512)})))
        .unwrap();
    let policy = ContextPolicy {
        offload_bytes: 4096,
        recent_exchanges: 1,
        excerpt_bytes: 256,
        page_bytes: 512,
        ..Default::default()
    };
    let reader = context::register(&mut registry, store, "demo", "context-read", &policy).unwrap();
    (registry, reader, policy)
}

fn exercise(connect: impl Fn() -> Store) {
    let actor = fixtures::actor();
    let store = connect();
    let (tools, reader, policy) = registry(store.clone());
    let (mut agent, mut definitions) = fixtures::definitions();
    agent.input_schema = None;
    agent.output_schema = None;
    agent.tools.truncate(1);
    agent.tools.push(AgentTool {
        tool_ref: reader.reference(),
        alias: reader.name.clone(),
    });
    agent.limits.max_context_bytes = 128 * 1024;
    agent.limits.max_model_calls = 12;
    agent.limits.max_harness_steps = 100;
    agent.limits.max_operations = 100;
    definitions.truncate(1);
    definitions.push(reader);
    for tool in definitions {
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
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut host = Runtime::new(store.clone(), AgentLoop, Cycle(requests.clone()), tools);
    let id = host
        .submit(&actor, fixtures::agent_ref(), json!("original"), None)
        .unwrap();
    for _ in 0..100 {
        host.tick(&actor, id).unwrap();
        if requests.lock().unwrap().len() == 5 {
            break;
        }
    }
    assert_eq!(requests.lock().unwrap().len(), 5);
    let compact = store
        .enqueue_message(&actor, id, MessageKind::Compact, Value::Null, "compact")
        .unwrap();
    let steer = store
        .enqueue_message(
            &actor,
            id,
            MessageKind::Steer,
            json!("new direction"),
            "steer",
        )
        .unwrap();
    assert!(store
        .enqueue_message(
            &actor,
            id,
            MessageKind::Compact,
            json!("not a command argument"),
            "bad"
        )
        .is_err());
    let other = Actor {
        id: "other".into(),
        ..actor.clone()
    };
    assert!(store
        .enqueue_message(&other, id, MessageKind::Compact, Value::Null, "foreign")
        .is_err());
    drop(host);
    drop(store);
    let store = connect();
    assert_eq!(store.queued_messages(&actor, id).unwrap().len(), 2);
    let (tools, _, _) = registry(store.clone());
    let mut host = Runtime::new(store.clone(), AgentLoop, Cycle(requests.clone()), tools);
    for _ in 0..100 {
        if host.tick(&actor, id).unwrap().status.terminal() {
            break;
        }
    }
    let view = store.inspect(&actor, id).unwrap();
    assert_eq!(view.status, RunStatus::Completed);
    assert_eq!(view.result, Some(json!("answer")));
    assert_eq!(view.usage.model_calls, 6);
    assert_eq!(view.usage.tool_calls, 5);
    assert!(store.queued_messages(&actor, id).unwrap().is_empty());
    assert_eq!(
        store
            .enqueue_message(&actor, id, MessageKind::Compact, Value::Null, "compact")
            .unwrap(),
        compact
    );
    let events = store.events(&actor, id, 0).unwrap();
    let reports: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == EventType::ContextCompacted)
        .collect();
    assert_eq!(reports.len(), 1);
    assert!(reports[0].payload["archived_exchanges"].as_u64().unwrap() > 0);
    let artifact = reports[0].payload["artifact_id"].as_str().unwrap();
    assert!(store
        .read_context_artifact(&actor, id, artifact, 0, 512)
        .is_ok());
    assert!(store
        .read_context_artifact(&other, id, artifact, 0, 512)
        .is_err());
    let consumption = events
        .iter()
        .find(|event| {
            event.event_type == EventType::InputConsumed
                && event.payload["message_ids"]
                    .as_array()
                    .is_some_and(|ids| ids.contains(&json!(compact.id)))
        })
        .unwrap();
    assert!(consumption.payload["message_ids"]
        .as_array()
        .unwrap()
        .contains(&json!(steer.id)));
    let requests = requests.lock().unwrap();
    assert!(requests[5]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|content| matches!(content,Content::Json{value} if value=="new direction")));
    assert!(!requests[5]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|content| matches!(content,Content::Json{value} if value.is_null())));
}

#[test]
fn compaction_and_direction_are_atomic_and_do_not_add_model_calls() {
    let store = Store::default();
    exercise(|| store.clone());
}

#[test]
#[ignore = "requires HUDSON_TEST_DATABASE in local PostgreSQL"]
fn compaction_survives_postgres_reopen() {
    let database = std::env::var("HUDSON_TEST_DATABASE").unwrap();
    let namespace = format!("compact-{}", uuid::Uuid::new_v4());
    exercise(|| Store::postgres_local("/tmp", &database, &namespace).unwrap());
}
