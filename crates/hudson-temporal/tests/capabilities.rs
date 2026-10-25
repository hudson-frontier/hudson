//! Combined customer capability flow through a real Temporal worker and PostgreSQL.
//! Provider/MCP responses are deterministic local fixtures; orchestration and
//! runtime policy, storage, context, memory, skills and verification are real.
use hudson_core::{
    configured::Configuration,
    memory::{MemoryKind, MemoryScope, MemorySource, RetainMemory},
    models::{Actor, OperationRequest, OperationResult, OperationStatus, RunStatus},
    storage::Store,
};
use hudson_temporal::{RunActivities, RunWorkflow};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use temporalio_client::{WorkflowGetResultOptions, WorkflowStartOptions};
use temporalio_sdk::{
    testing::{LocalWorkflowEnvironmentOptions, WorkflowEnvironment},
    Runtime, Worker, WorkerOptions,
};

#[derive(Default)]
struct Observed {
    model_calls: usize,
    child_model_calls: usize,
    delegated: bool,
    joined: bool,
    recalled_seed: bool,
    loaded_skill_resource: bool,
    offloaded: bool,
    archived: bool,
    source: String,
    artifact_id: Option<String>,
    incorrect_candidate_sent: bool,
    corrected_candidate_sent: bool,
}
struct Fixture {
    endpoint: String,
    observed: Arc<Mutex<Observed>>,
    mcp_calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.server_addr());
        let observed = Arc::new(Mutex::new(Observed::default()));
        let mcp_calls = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (state, calls, done) = (observed.clone(), mcp_calls.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let Some(mut request) = server.recv_timeout(Duration::from_millis(100)).unwrap()
                else {
                    continue;
                };
                let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
                if request.url() == "/model" && body["model"] == "child-fixture" {
                    state.lock().unwrap().child_model_calls += 1;
                    let response = json!({"choices":[{"message":{"role":"assistant","content":json!({"checked":true,"record":"inventory-row-42"}).to_string()},"finish_reason":"stop"}]});
                    request
                        .respond(
                            tiny_http::Response::from_string(response.to_string()).with_header(
                                tiny_http::Header::from_bytes("Content-Type", "application/json")
                                    .unwrap(),
                            ),
                        )
                        .unwrap();
                    continue;
                }
                let response = if request.url() == "/mcp" {
                    let result = match body["method"].as_str().unwrap() {
                        "initialize" => {
                            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"inventory","version":"1"}})
                        }
                        "notifications/initialized" => {
                            request.respond(tiny_http::Response::empty(202)).unwrap();
                            continue;
                        }
                        "tools/list" => {
                            json!({"tools":[{"name":"inventory","description":"Read inventory","inputSchema":schema()}]})
                        }
                        "tools/call" => {
                            assert_eq!(body["params"]["name"], "inventory");
                            assert!(body["params"]["_meta"]["hudson/operation_id"].is_string());
                            calls.fetch_add(1, Ordering::SeqCst);
                            json!({"content":[{"type":"text","text":"Inventory source is attached"}],"structuredContent":{"answer":42,"record_id":"inventory-row-42","data":"x".repeat(4500)},"isError":false})
                        }
                        method => panic!("unexpected MCP method {method}"),
                    };
                    json!({"jsonrpc":"2.0","id":body["id"],"result":result})
                } else {
                    assert_eq!(request.url(), "/model");
                    let mut state = state.lock().unwrap();
                    state.model_calls += 1;
                    assert!(state.model_calls < 40, "capability loop made no progress");
                    let encoded = body.to_string();
                    if state.model_calls == 1 {
                        state.recalled_seed = encoded
                            .contains("Austin metric seed: warehouse counts use active inventory");
                        assert!(
                            state.recalled_seed,
                            "scoped seed memory missing from first prompt"
                        );
                    }
                    state.archived |= encoded.contains("Earlier conversation is archived");
                    let last = body["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .rev()
                        .find(|message| message["role"] == "tool")
                        .map(|message| {
                            serde_json::from_str::<Value>(message["content"].as_str().unwrap())
                                .unwrap()
                        });
                    let value = last.as_ref().map(|result| &result["value"]);
                    let action = match state.model_calls {
                        1 => Some(("load_skill", json!({"name":"analysis"}))),
                        2 => Some((
                            "load_skill",
                            json!({"name":"analysis","resource":"references/rules.md"}),
                        )),
                        3 => {
                            state.loaded_skill_resource = value.unwrap()["contents"]
                                == "Use active inventory and cite record identifiers.";
                            assert!(state.loaded_skill_resource);
                            Some(("inventory", json!({})))
                        }
                        _ if value.is_some_and(|v| v.get("context_artifact").is_some()) => {
                            let artifact = value.unwrap()["context_artifact"]["artifact_id"]
                                .as_str()
                                .unwrap()
                                .to_owned();
                            state.offloaded = true;
                            state.artifact_id = Some(artifact.clone());
                            Some(("read_context_artifact", json!({"artifact_id":artifact})))
                        }
                        _ if state.delegated && !state.joined => {
                            assert_eq!(
                                state.child_model_calls, 1,
                                "lead continued before child completed"
                            );
                            state.joined = true;
                            Some((
                                "join_delegate_checker",
                                json!({"run_id":value.unwrap()["child_run"]}),
                            ))
                        }
                        _ if state.joined && !state.incorrect_candidate_sent => {
                            assert_eq!(value.unwrap()["status"], "completed");
                            assert_eq!(value.unwrap()["result"]["checked"], true);
                            None
                        }
                        _ if !state.incorrect_candidate_sent => {
                            let page = value.unwrap();
                            state.source.push_str(page["text"].as_str().unwrap());
                            if let Some(offset) = page["next_offset"].as_u64() {
                                Some((
                                    "read_context_artifact",
                                    json!({"artifact_id":state.artifact_id,"offset":offset}),
                                ))
                            } else {
                                let source: Value = serde_json::from_str(&state.source).unwrap();
                                assert_eq!(source["value"]["structuredContent"]["answer"], 42);
                                assert_eq!(
                                    source["value"]["structuredContent"]["record_id"],
                                    "inventory-row-42"
                                );
                                state.delegated = true;
                                Some((
                                    "delegate_checker",
                                    json!({"task":"Check the inventory-row-42 result of 42 active warehouses"}),
                                ))
                            }
                        }
                        _ => None,
                    };
                    let message = if let Some((name, args)) = action {
                        json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call-{}",state.model_calls),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
                    } else {
                        let total = if state.incorrect_candidate_sent {
                            state.corrected_candidate_sent = true;
                            42
                        } else {
                            state.incorrect_candidate_sent = true;
                            41
                        };
                        json!({"role":"assistant","content":json!({"total":total,"memo":"Austin warehouse verified inventory total 42","evidence":["inventory-row-42"]}).to_string()})
                    };
                    let finish = if message.get("tool_calls").is_some() {
                        "tool_calls"
                    } else {
                        "stop"
                    };
                    json!({"choices":[{"message":message,"finish_reason":finish}]})
                };
                request
                    .respond(
                        tiny_http::Response::from_string(response.to_string()).with_header(
                            tiny_http::Header::from_bytes("Content-Type", "application/json")
                                .unwrap(),
                        ),
                    )
                    .unwrap();
            }
        });
        Self {
            endpoint,
            observed,
            mcp_calls,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}
fn schema() -> Value {
    json!({"type":"object","properties":{},"additionalProperties":false})
}

#[test]
#[ignore = "starts real Temporal and requires local PostgreSQL"]
fn temporal_combines_customer_capabilities_and_persists_verified_evidence() {
    let fixture = Fixture::new();
    let directory = tempfile::tempdir().unwrap();
    let skill = directory.path().join("analysis");
    std::fs::create_dir_all(skill.join("references")).unwrap();
    std::fs::write(skill.join("SKILL.md"),"---\nname: analysis\ndescription: Analyze inventory evidence\n---\nRead references/rules.md before inventory analysis.").unwrap();
    std::fs::write(
        skill.join("references/rules.md"),
        "Use active inventory and cite record identifiers.",
    )
    .unwrap();
    let config_file = directory.path().join("agent.json");
    std::fs::write(&config_file,json!({
        "name":"capabilities","instructions":"Analyze Austin warehouse inventory and verify every conclusion.",
        "model":"fixture","endpoint":format!("{}/model",fixture.endpoint),
        "skill_packages":["analysis"],
        "shared_model_budget":{"group":"capabilities-team","limit":60},
        "subagents":[{"name":"checker","instructions":"Check the supplied inventory result and return checked with record identifier.","model":"child-fixture","endpoint":format!("{}/model",fixture.endpoint),"output_schema":{"type":"object","properties":{"checked":{"const":true},"record":{"const":"inventory-row-42"}},"required":["checked","record"]}}],
        "memory":{"scope":{"name":"inventory"},"recall_limit":3,"retain_pointer":"/memo"},
        "context":{"offload_bytes":4096,"recent_exchanges":1,"excerpt_bytes":256,"page_bytes":512},
        "limits":{"max_context_bytes":10000,"max_model_calls":50,"max_harness_steps":200,"max_operations":120},
        "goal":{"objective":"Verify warehouse inventory count","success_schema":{"type":"object","required":["total","memo","evidence"]},"criteria":[{"type":"tool_result_equals","tool_name":"inventory","arguments":{},"pointer":"/structuredContent/answer","expected":42},{"type":"equals","pointer":"/total","expected":42},{"type":"contains","pointer":"/evidence/0","text":"inventory-row-42"}]},
        "mcp_servers":[{"endpoint":format!("{}/mcp",fixture.endpoint),"timeout_seconds":5,"tools":[{"name":"inventory","remote_name":"inventory","description":"Read active inventory source data","input_schema":schema(),"effect":"read"}]}]
    }).to_string()).unwrap();
    let database = std::env::var("HUDSON_TEST_DATABASE")
        .unwrap_or_else(|_| "hudson_harness_test_20260921".into());
    let namespace = format!("temporal-capabilities-{}", uuid::Uuid::new_v4());
    let actor = Actor {
        workspace_id: "customer".into(),
        id: "owner".into(),
    };
    let scope = MemoryScope {
        name: "inventory".into(),
    };
    let store = Store::postgres_local("/tmp", &database, &namespace).unwrap();
    store
        .retain_memory(
            &actor,
            &scope,
            "seed",
            RetainMemory {
                text: "Austin metric seed: warehouse counts use active inventory".into(),
                kind: MemoryKind::UserFact,
                sources: vec![MemorySource {
                    reference: "customer:metric-definition".into(),
                    run_id: None,
                    operation_id: None,
                }],
                supersedes: None,
            },
        )
        .unwrap();
    let tree = Configuration::load(&config_file)
        .unwrap()
        .build_temporal_tree(store.clone(), &actor)
        .unwrap();
    let queue = format!("capabilities-{}", uuid::Uuid::new_v4());
    let target = hudson_temporal::ExecutionClient::schedule_target(&namespace, &queue);
    let run = tree
        .runtime
        .submit_scheduled(
            &actor,
            tree.reference.clone(),
            json!("Analyze Austin warehouse inventory"),
            None,
            tree.goal.clone(),
            Some(target.clone()),
        )
        .unwrap();
    let observed = fixture.observed.clone();
    let actor_for_worker = actor.clone();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            let env = WorkflowEnvironment::start_local(LocalWorkflowEnvironmentOptions::default())
                .await
                .unwrap();
            let runtime = Runtime::from_current_tokio(Default::default()).unwrap();
            let options = WorkerOptions::new(queue.clone())
                .register_workflow::<RunWorkflow>()
                .unwrap()
                .register_activities(RunActivities::new(
                    tree.into_scheduled(),
                    actor_for_worker,
                    target,
                ))
                .build();
            let mut worker = Worker::new(&runtime, env.client().clone(), options).unwrap();
            let stop = worker.shutdown_handle();
            let handle = env
                .client()
                .start_workflow(
                    RunWorkflow::run,
                    run,
                    WorkflowStartOptions::new(queue, format!("capabilities-{run}")).build(),
                )
                .await
                .unwrap();
            let (worker_result, result) = tokio::join!(worker.run(), async {
                let result = tokio::time::timeout(
                    Duration::from_secs(180),
                    handle.get_result(WorkflowGetResultOptions::default()),
                )
                .await;
                stop();
                result
            });
            worker_result.unwrap();
            env.shutdown().await.unwrap();
            assert_eq!(result.unwrap().unwrap().status, RunStatus::Completed);
        });
    drop(store);
    // Open a fresh DB connection and rebuild definitions as a replacement host.
    let reopened = Store::postgres_local("/tmp", &database, &namespace).unwrap();
    let mut replacement = Configuration::load(&config_file)
        .unwrap()
        .build_temporal_tree(reopened.clone(), &actor)
        .unwrap();
    assert_eq!(
        replacement.tick(&actor, run).unwrap().status,
        RunStatus::Completed
    );
    assert_eq!(fixture.mcp_calls.load(Ordering::SeqCst), 1);
    let view = reopened.inspect(&actor, run).unwrap();
    assert_eq!(view.result.as_ref().unwrap()["total"], 42);
    assert!(view.assessment.as_ref().unwrap().passed);
    assert!(view
        .assessment
        .as_ref()
        .unwrap()
        .evidence
        .iter()
        .any(|e| e.tool_name == "inventory"));
    let operations = reopened.operations(&actor, run).unwrap();
    let verification = operations
        .iter()
        .filter_map(|op| match &op.result {
            Some(OperationResult::Verify { passed, .. }) => Some(*passed),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(verification, vec![false, true]);
    assert!(operations.iter().any(|op|matches!(&op.request,OperationRequest::Tool{call,..} if call.name=="read_context_artifact") && op.status==OperationStatus::Succeeded));
    let retained = reopened
        .recall_memory(&actor, &scope, "Austin warehouse verified", 20)
        .unwrap();
    assert_eq!(
        retained
            .iter()
            .filter(|memory| memory.content.kind == MemoryKind::SuccessfulOutcome)
            .count(),
        1
    );
    let outcome = retained
        .iter()
        .find(|memory| memory.content.kind == MemoryKind::SuccessfulOutcome)
        .unwrap();
    assert_eq!(
        outcome.content.text,
        "Austin warehouse verified inventory total 42"
    );
    assert_eq!(outcome.content.sources[0].run_id, Some(run));
    let observed = observed.lock().unwrap();
    assert!(
        observed.recalled_seed
            && observed.loaded_skill_resource
            && observed.offloaded
            && observed.archived
    );
    assert!(observed.incorrect_candidate_sent && observed.corrected_candidate_sent);
    assert!(observed.delegated && observed.joined);
    assert_eq!(observed.child_model_calls, 1);
    let children = reopened.children(&actor, run).unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].status, RunStatus::Completed);
}
