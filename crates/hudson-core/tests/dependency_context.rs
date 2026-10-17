#![cfg(feature = "fixtures")]
use hudson_core::{
    adapters::{
        models::ModelExecutor,
        tools::{ExecutionError, ToolRegistry},
    },
    context::{self, ContextPolicy},
    coordination::CoordinationPolicy,
    fixtures,
    models::*,
    runtime::Runtime,
    security::Policy,
    storage::Store,
    subagents,
};
use hudson_harness::{AgentLoop, Content, ModelRequest, ModelResponse, ToolCall, ToolOutcome};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

struct Model {
    calls: Vec<ToolCall>,
    output: Value,
}
impl ModelExecutor for Model {
    fn call(&mut self, _: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        if self.calls.is_empty() {
            Ok(ModelResponse::Final {
                output: self.output.clone(),
            })
        } else {
            Ok(ModelResponse::ToolCalls {
                calls: std::mem::take(&mut self.calls),
            })
        }
    }
}
fn model(calls: Vec<ToolCall>, output: Value) -> Model {
    Model { calls, output }
}
fn call(name: &str, key: &str, depends_on: Vec<&str>) -> ToolCall {
    ToolCall {
        call_id: key.into(),
        name: name.into(),
        arguments: json!({"task":key,"task_key":key,"depends_on":depends_on}),
        provider_metadata: Value::Null,
    }
}
fn publish(
    store: &Store,
    name: &str,
    tools: Vec<Tool>,
    context: Option<ContextPolicy>,
    max_context_bytes: usize,
    instructions: &str,
) -> VersionRef {
    let (mut agent, _) = fixtures::definitions();
    agent.id = name.into();
    agent.name = name.into();
    agent.input_schema = None;
    agent.output_schema = None;
    agent.instructions = instructions.into();
    agent.limits.max_context_bytes = max_context_bytes;
    agent.limits.max_model_calls = 100;
    agent.limits.max_operations = 250;
    agent.limits.max_harness_steps = 500;
    agent.tools = tools
        .iter()
        .map(|tool| AgentTool {
            tool_ref: tool.reference(),
            alias: tool.name.clone(),
        })
        .collect();
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
    let reference = agent.reference();
    store.publish_agent(agent).unwrap();
    store
        .bind_context_policy("demo", &reference, context.as_ref())
        .unwrap();
    reference
}
fn reader(store: Store) -> (ToolRegistry, Tool, ContextPolicy) {
    let policy = ContextPolicy {
        reserve_bytes: 0,
        offload_bytes: 4096,
        excerpt_bytes: 256,
        recent_exchanges: 1,
        page_bytes: 512,
    };
    let mut registry = ToolRegistry::new();
    let tool = context::register(&mut registry, store, "demo", "read-context", &policy).unwrap();
    (registry, tool, policy)
}
fn finish<M: ModelExecutor>(
    runtime: &mut Runtime<AgentLoop, M, ToolRegistry>,
    id: Uuid,
) -> RunView {
    for _ in 0..400 {
        let view = runtime.tick(&fixtures::actor(), id).unwrap();
        if view.status.terminal() {
            return view;
        }
    }
    panic!("run did not settle");
}
fn batch<M: ModelExecutor>(runtime: &mut Runtime<AgentLoop, M, ToolRegistry>, id: Uuid) {
    for _ in 0..20 {
        runtime.tick(&fixtures::actor(), id).unwrap();
        let ops = runtime.store.operations(&fixtures::actor(), id).unwrap();
        if ops.len() > 1
            && ops.iter().all(|op| {
                !matches!(
                    op.status,
                    OperationStatus::Pending
                        | OperationStatus::Running
                        | OperationStatus::Unknown
                        | OperationStatus::WaitingApproval
                )
            })
        {
            return;
        }
    }
    panic!("batch did not settle");
}
struct Reader {
    source: String,
    calls: usize,
    artifact: Arc<Mutex<Option<String>>>,
}
impl ModelExecutor for Reader {
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        assert!(serde_json::to_vec(request).unwrap().len() <= 6500);
        let index = request
            .instructions
            .rsplit_once("source run IDs identify persisted evidence): ")
            .unwrap()
            .1;
        let index: Value = serde_json::from_str(index).unwrap();
        let id = index["context_artifact"]["artifact_id"].as_str().unwrap();
        *self.artifact.lock().unwrap() = Some(id.into());
        let last = request
            .messages
            .iter()
            .rev()
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                Content::ToolResult { result } => match &result.outcome {
                    ToolOutcome::Success { value } => Some(value),
                    _ => panic!("artifact reader failed"),
                },
                _ => None,
            });
        let offset = if let Some(last) = last {
            self.source.push_str(last["text"].as_str().unwrap());
            match last["next_offset"].as_u64() {
                Some(offset) => offset,
                None => {
                    let original: Value = serde_json::from_str(&self.source).unwrap();
                    assert_eq!(original["kind"], "dependency_results");
                    assert_eq!(
                        original["value"][0]["result"]["sentinel"],
                        "complete original evidence"
                    );
                    assert_eq!(
                        original["value"][0]["result"]["large"]
                            .as_str()
                            .unwrap()
                            .len(),
                        14000
                    );
                    return Ok(ModelResponse::Final {
                        output: json!({"verified":true}),
                    });
                }
            }
        } else {
            0
        };
        self.calls += 1;
        Ok(ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                call_id: format!("read-{}", self.calls),
                name: "read_context_artifact".into(),
                arguments: json!({"artifact_id":id,"offset":offset}),
                provider_metadata: Value::Null,
            }],
        })
    }
}
struct Setup {
    store: Store,
    source: Uuid,
    sink: Uuid,
}
fn setup(enabled: bool, impossible: bool) -> Setup {
    setup_store(enabled, impossible, Store::default())
}
fn setup_store(enabled: bool, impossible: bool, store: Store) -> Setup {
    let actor = fixtures::actor();
    let source_ref = publish(&store, "source", vec![], None, 64000, "produce evidence");
    let (sink_registry, sink_tools, policy) = if enabled {
        let (registry, tool, policy) = reader(store.clone());
        (registry, vec![tool], Some(policy))
    } else {
        (ToolRegistry::new(), vec![], None)
    };
    let instructions = if impossible {
        "fixed ".repeat(2000)
    } else {
        "read prerequisite evidence".into()
    };
    let sink_ref = publish(&store, "sink", sink_tools, policy, 6500, &instructions);
    let mut registry = ToolRegistry::new();
    let mut tools = vec![];
    for (name, reference, tools_registry) in [
        ("delegate_source", source_ref.clone(), ToolRegistry::new()),
        ("delegate_sink", sink_ref, sink_registry),
    ] {
        let runtime = Runtime::new(
            store.clone(),
            AgentLoop,
            model(vec![], Value::Null),
            tools_registry,
        );
        tools.extend(
            subagents::register_deferred_with_join(
                &mut registry,
                Arc::new(Mutex::new(runtime)),
                actor.clone(),
                reference,
                name,
                "delegate",
                "delegate",
            )
            .unwrap(),
        );
    }
    let lead = publish(&store, "lead", tools, None, 64000, "coordinate");
    store
        .bind_coordination_policy(
            "demo",
            &lead,
            &CoordinationPolicy {
                child_max_model_calls: 100,
                child_max_operations: 250,
                child_max_harness_steps: 500,
                ..Default::default()
            },
        )
        .unwrap();
    let mut root = Runtime::new(
        store.clone(),
        AgentLoop,
        model(
            vec![
                call("delegate_source", "source", vec![]),
                call("delegate_sink", "sink", vec!["source"]),
            ],
            json!("done"),
        ),
        registry,
    );
    let root_id = root.submit(&actor, lead, json!("work"), None).unwrap();
    batch(&mut root, root_id);
    let children = store.children(&actor, root_id).unwrap();
    let source = children
        .iter()
        .find(|child| child.agent_ref == source_ref)
        .unwrap()
        .id;
    let sink = children.iter().find(|child| child.id != source).unwrap().id;
    Setup {
        store,
        source,
        sink,
    }
}
fn large_prerequisite(connect: impl Fn() -> Store) {
    let Setup {
        store,
        source,
        sink,
        ..
    } = setup_store(true, false, connect());
    let mut producer = Runtime::new(
        store.clone(),
        AgentLoop,
        model(
            vec![],
            json!({"large":"x".repeat(14000),"sentinel":"complete original evidence"}),
        ),
        ToolRegistry::new(),
    );
    assert_eq!(finish(&mut producer, source).status, RunStatus::Completed);
    drop(producer);
    // Commit the receiving run's model request and staged artifact before restart.
    let (registry, _, _) = reader(store.clone());
    let mut initial = Runtime::new(
        store.clone(),
        AgentLoop,
        model(vec![], Value::Null),
        registry,
    );
    initial.tick(&fixtures::actor(), sink).unwrap();
    drop(initial);
    drop(store);
    let store = connect();
    let (registry, _, _) = reader(store.clone());
    let artifact = Arc::new(Mutex::new(None));
    let mut consumer = Runtime::new(
        store.clone(),
        AgentLoop,
        Reader {
            source: String::new(),
            calls: 0,
            artifact: artifact.clone(),
        },
        registry,
    );
    assert_eq!(finish(&mut consumer, sink).status, RunStatus::Completed);
    let id = artifact.lock().unwrap().clone().unwrap();
    assert!(store
        .read_context_artifact(&fixtures::actor(), sink, &id, 0, 512)
        .is_ok());
    assert!(store
        .read_context_artifact(&fixtures::actor(), source, &id, 0, 512)
        .is_err());
    assert!(store
        .read_context_artifact(
            &Actor {
                id: "different-actor".into(),
                ..fixtures::actor()
            },
            sink,
            &id,
            0,
            512
        )
        .is_err());
}
#[test]
fn large_prerequisite_is_read_through_artifacts_in_actual_child_loop() {
    let store = Store::default();
    large_prerequisite(|| store.clone());
}
#[test]
#[ignore = "requires HUDSON_TEST_DATABASE in local PostgreSQL"]
fn prerequisite_artifact_survives_postgres_reconnect_before_model_dispatch() {
    let database = std::env::var("HUDSON_TEST_DATABASE").unwrap();
    let namespace = format!("dependency-artifact-{}", Uuid::new_v4());
    large_prerequisite(|| Store::postgres_local("/tmp", &database, &namespace).unwrap());
}

#[test]
fn context_disabled_or_impossible_fixed_context_fails_explicitly() {
    for (enabled, impossible) in [(false, false), (true, true)] {
        let Setup {
            store,
            source,
            sink,
            ..
        } = setup(enabled, impossible);
        let mut producer = Runtime::new(
            store.clone(),
            AgentLoop,
            model(vec![], json!({"large":"x".repeat(14000)})),
            ToolRegistry::new(),
        );
        assert_eq!(finish(&mut producer, source).status, RunStatus::Completed);
        let mut consumer = Runtime::new(
            store.clone(),
            AgentLoop,
            model(vec![], Value::Null),
            ToolRegistry::new(),
        );
        let result = finish(&mut consumer, sink);
        assert_eq!(result.status, RunStatus::Failed);
        assert!(result.reason.unwrap().contains("context"));
        assert!(store
            .operations(&fixtures::actor(), sink)
            .unwrap()
            .is_empty());
        let source_view = store.inspect(&fixtures::actor(), source).unwrap();
        let content = json!({"kind":"dependency_results","value":[{"run_id":source,"task_key":"source","result":source_view.result,"assessment":source_view.assessment}]});
        let expected_id = hudson_core::definitions::digest(&("demo", sink, &content)).unwrap();
        assert!(
            store
                .read_context_artifact(&fixtures::actor(), sink, &expected_id, 0, 512)
                .is_err(),
            "failed advancement must not commit staged artifacts"
        );
    }
}

#[test]
fn nested_delegation_obeys_root_budget_and_root_terminal_state() {
    for stop_root in [false, true] {
        let store = Store::default();
        let actor = fixtures::actor();
        let leaf_ref = publish(&store, "leaf", vec![], None, 64000, "leaf");
        let leaf = Runtime::new(
            store.clone(),
            AgentLoop,
            model(vec![], json!("leaf result")),
            ToolRegistry::new(),
        );
        let mut middle_registry = ToolRegistry::new();
        let middle_tools = subagents::register_deferred_with_join(
            &mut middle_registry,
            Arc::new(Mutex::new(leaf)),
            actor.clone(),
            leaf_ref,
            "delegate_leaf",
            "leaf",
            "delegate",
        )
        .unwrap();
        let middle_ref = publish(
            &store,
            "middle",
            middle_tools,
            None,
            64000,
            "delegate leaf tasks",
        );
        store
            .bind_coordination_policy("demo", &middle_ref, &CoordinationPolicy::default())
            .unwrap();
        let middle = Arc::new(Mutex::new(Runtime::new(
            store.clone(),
            AgentLoop,
            model(
                vec![
                    call("delegate_leaf", "one", vec![]),
                    call("delegate_leaf", "two", vec![]),
                ],
                json!("middle result"),
            ),
            middle_registry,
        )));
        let mut root_registry = ToolRegistry::new();
        let root_tools = subagents::register_deferred_with_join(
            &mut root_registry,
            middle.clone(),
            actor.clone(),
            middle_ref.clone(),
            "delegate_middle",
            "middle",
            "delegate",
        )
        .unwrap();
        let root_ref = publish(&store, "root", root_tools, None, 64000, "delegate middle");
        store
            .bind_coordination_policy(
                "demo",
                &root_ref,
                &CoordinationPolicy {
                    max_children: 2,
                    max_repeated_task: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut root = Runtime::new(
            store.clone(),
            AgentLoop,
            model(
                vec![call("delegate_middle", "middle", vec![])],
                json!("root result"),
            ),
            root_registry,
        );
        let root_id = root
            .submit(&actor, root_ref, json!("root task"), None)
            .unwrap();
        batch(&mut root, root_id);
        let middle_id = store.children(&actor, root_id).unwrap()[0].id;
        let team_task = store.team_task(&actor, middle_id).unwrap().unwrap();
        for wrong_actor in [
            Actor {
                id: "other".into(),
                ..actor.clone()
            },
            Actor {
                workspace_id: "other".into(),
                ..actor.clone()
            },
        ] {
            assert!(middle
                .lock()
                .unwrap()
                .submit_delegated(
                    &wrong_actor,
                    middle_ref.clone(),
                    json!("middle"),
                    team_task.parent_operation,
                    hudson_core::coordination::DelegationOptions {
                        task_key: Some("middle".into()),
                        ..Default::default()
                    }
                )
                .is_err());
        }
        if stop_root {
            assert_eq!(finish(&mut root, root_id).status, RunStatus::Completed);
        }
        batch(&mut middle.lock().unwrap(), middle_id);
        let grandchildren = store.children(&actor, middle_id).unwrap();
        assert_eq!(grandchildren.len(), if stop_root { 0 } else { 1 });
        for child in grandchildren {
            assert_eq!(
                store.team_task(&actor, child.id).unwrap().unwrap().root_run,
                root_id
            );
        }
        let encoded = serde_json::to_string(&store.operations(&actor, middle_id).unwrap()).unwrap();
        assert!(encoded.contains(if stop_root {
            "root is no longer active"
        } else {
            "delegation budget exhausted"
        }));
    }
}
struct Uncertain;
impl ModelExecutor for Uncertain {
    fn call(&mut self, _: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        Err(ExecutionError::Unknown(
            "provider acknowledgement lost".into(),
        ))
    }
}
#[test]
fn uncertain_prerequisite_remains_unresolved_without_replay_or_dependent_effects() {
    let Setup {
        store,
        source,
        sink,
        ..
    } = setup(false, false);
    let mut producer = Runtime::new(store.clone(), AgentLoop, Uncertain, ToolRegistry::new());
    producer.tick(&fixtures::actor(), source).unwrap();
    producer.tick(&fixtures::actor(), source).unwrap();
    let ops = store.operations(&fixtures::actor(), source).unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].status, OperationStatus::Unknown);
    let mut consumer = Runtime::new(
        store.clone(),
        AgentLoop,
        model(vec![], json!("must not run")),
        ToolRegistry::new(),
    );
    for _ in 0..3 {
        producer.tick(&fixtures::actor(), source).unwrap();
        assert_eq!(
            consumer.tick(&fixtures::actor(), sink).unwrap().status,
            RunStatus::Queued
        );
    }
    assert!(store
        .operations(&fixtures::actor(), sink)
        .unwrap()
        .is_empty());
    assert_eq!(
        store.operations(&fixtures::actor(), source).unwrap()[0]
            .attempts
            .len(),
        1
    );
}

struct PausedLoop {
    entered: std::sync::mpsc::Sender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl hudson_harness::Backend for PausedLoop {
    fn name(&self) -> &str {
        "hudson"
    }
    fn version(&self) -> u32 {
        1
    }
    fn advance(
        &self,
        config: &hudson_harness::Config,
        state: &hudson_harness::Checkpoint,
        input: hudson_harness::Input,
    ) -> Result<hudson_harness::Transition, hudson_harness::HarnessError> {
        self.entered.send(()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        hudson_harness::Backend::advance(&AgentLoop, config, state, input)
    }
}
#[test]
fn cancellation_during_preparation_rolls_back_staged_dependency_artifact() {
    let Setup {
        store,
        source,
        sink,
        ..
    } = setup(true, false);
    let mut producer = Runtime::new(
        store.clone(),
        AgentLoop,
        model(vec![], json!({"large":"x".repeat(14000)})),
        ToolRegistry::new(),
    );
    assert_eq!(finish(&mut producer, source).status, RunStatus::Completed);
    let (entered, ready) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let runtime_store = store.clone();
    let worker = std::thread::spawn(move || {
        let mut runtime = Runtime::new(
            runtime_store,
            PausedLoop {
                entered,
                release: Mutex::new(wait),
            },
            model(vec![], Value::Null),
            ToolRegistry::new(),
        );
        runtime.tick(&fixtures::actor(), sink)
    });
    ready
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    producer.cancel(&fixtures::actor(), sink).unwrap();
    release.send(()).unwrap();
    assert!(worker.join().unwrap().is_err());
    assert_eq!(
        store.inspect(&fixtures::actor(), sink).unwrap().status,
        RunStatus::Cancelled
    );
    let source_view = store.inspect(&fixtures::actor(), source).unwrap();
    let content = json!({"kind":"dependency_results","value":[{"run_id":source,"task_key":"source","result":source_view.result,"assessment":source_view.assessment}]});
    let expected_id = hudson_core::definitions::digest(&("demo", sink, &content)).unwrap();
    assert!(store
        .read_context_artifact(&fixtures::actor(), sink, &expected_id, 0, 512)
        .is_err());
    assert!(store
        .operations(&fixtures::actor(), sink)
        .unwrap()
        .is_empty());
}
