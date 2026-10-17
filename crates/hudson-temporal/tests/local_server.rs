use hudson_core::{
    configured::Configuration,
    models::{Actor, RunStatus},
    storage::Store,
};
use hudson_temporal::{RunActivities, RunWorkflow};

use temporalio_client::{WorkflowGetResultOptions, WorkflowStartOptions};
use temporalio_sdk::{
    testing::{LocalWorkflowEnvironmentOptions, WorkflowEnvironment},
    Runtime, Worker, WorkerOptions,
};

#[test]
#[ignore = "starts a local Temporal server (downloads the CLI on first use)"]
fn temporal_runs_existing_runtime_activity() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        file.path(),
        r#"{"name":"test","instructions":"help","endpoint":"http://127.0.0.1:1/chat"}"#,
    )
    .unwrap();
    let actor = Actor {
        workspace_id: "local".into(),
        id: "developer".into(),
    };
    let tree = Configuration::load(file.path())
        .unwrap()
        .build_tree(Store::default(), &actor)
        .unwrap();
    let id = tree
        .runtime
        .submit(
            &actor,
            tree.reference.clone(),
            serde_json::json!("test"),
            None,
        )
        .unwrap();
    tree.runtime.cancel(&actor, id).unwrap();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            let env = WorkflowEnvironment::start_local(LocalWorkflowEnvironmentOptions::default())
                .await
                .unwrap();
            let runtime = Runtime::from_current_tokio(Default::default()).unwrap();
            let options = WorkerOptions::new("hudson-test")
                .register_workflow::<RunWorkflow>()
                .unwrap()
                .register_activities(RunActivities::new(tree.into_scheduled(), actor))
                .build();
            let mut worker = Worker::new(&runtime, env.client().clone(), options).unwrap();
            let stop = worker.shutdown_handle();
            let handle = env
                .client()
                .start_workflow(
                    RunWorkflow::run,
                    id,
                    WorkflowStartOptions::new("hudson-test", format!("test-{id}")).build(),
                )
                .await
                .unwrap();
            let (worker_result, result) = tokio::join!(worker.run(), async {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    handle.get_result(WorkflowGetResultOptions::default()),
                )
                .await;
                stop();
                result
            });
            worker_result.unwrap();
            env.shutdown().await.unwrap();
            assert_eq!(result.unwrap().unwrap().status, RunStatus::Cancelled);
        });
}

#[test]
#[ignore = "starts a local Temporal server (downloads the CLI on first use)"]
fn team_children_run_concurrently_before_lead_continues() {
    team_scenario(false);
}

#[test]
#[ignore = "starts a local Temporal server"]
fn published_team_reconstructs_root_and_delegated_children() {
    team_scenario(true);
}

fn team_scenario(published: bool) {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/chat", server.server_addr());
    let barrier = Arc::new(AtomicUsize::new(0));
    let child_calls = Arc::new(AtomicUsize::new(0));
    let children_done = child_calls.clone();
    let server_thread = std::thread::spawn(move || {
        let mut root_calls = 0;
        let mut handlers = Vec::new();
        for _ in 0..6 {
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(40))
                .unwrap()
                .expect("model call timed out");
            let body: serde_json::Value = serde_json::from_reader(request.as_reader()).unwrap();
            let is_root = body["model"] == "root-model";
            let first = is_root && root_calls == 0;
            let join = is_root && root_calls == 1;
            if is_root {
                root_calls += 1;
            }
            let barrier = barrier.clone();
            let child_calls = child_calls.clone();
            handlers.push(std::thread::spawn(move || {
                let message = if first {
                    serde_json::json!({"role":"assistant","content":null,"tool_calls":[
                        {"id":"a","type":"function","function":{"name":"delegate_a","arguments":serde_json::json!({"task":"do a","task_key":"a"}).to_string()}},
                        {"id":"b","type":"function","function":{"name":"delegate_b","arguments":serde_json::json!({"task":"do b","task_key":"b"}).to_string()}},
                        {"id":"c","type":"function","function":{"name":"delegate_c","arguments":serde_json::json!({"task":"use a result","task_key":"c","depends_on":["a"]}).to_string()}}
                    ]})
                } else if join {
                    assert_eq!(child_calls.load(Ordering::SeqCst), 3);
                    let calls = ["a", "b", "c"].into_iter().map(|name| {
                        let receipt = body["messages"].as_array().unwrap().iter()
                            .find(|message| message["role"] == "tool" && message["tool_call_id"] == name).unwrap();
                        let receipt: serde_json::Value = serde_json::from_str(receipt["content"].as_str().unwrap()).unwrap();
                        serde_json::json!({"id":format!("join-{name}"),"type":"function","function":{
                            "name":format!("join_delegate_{name}"),"arguments":serde_json::json!({"run_id":receipt["value"]["child_run"]}).to_string()
                        }})
                    }).collect::<Vec<_>>();
                    serde_json::json!({"role":"assistant","content":null,"tool_calls":calls})
                } else {
                    if !is_root && body["model"] == "c-model" {
                        let instructions = body["messages"][0]["content"].as_str().unwrap();
                        assert!(instructions.contains("Completed prerequisite results"));
                        assert!(instructions.contains("done"));
                        child_calls.fetch_add(1, Ordering::SeqCst);
                    } else if !is_root {
                        barrier.fetch_add(1, Ordering::SeqCst);
                        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                        while barrier.load(Ordering::SeqCst) < 2 {
                            assert!(std::time::Instant::now() < deadline, "children were serialized");
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        child_calls.fetch_add(1, Ordering::SeqCst);
                    } else {
                        assert_eq!(child_calls.load(Ordering::SeqCst), 3, "lead advanced before its team");
                        for name in ["join-a", "join-b", "join-c"] {
                            let receipt = body["messages"].as_array().unwrap().iter().find(|m| m["tool_call_id"] == name).unwrap();
                            let receipt: serde_json::Value = serde_json::from_str(receipt["content"].as_str().unwrap()).unwrap();
                            assert_eq!(receipt["value"]["status"], "completed");
                            assert_eq!(receipt["value"]["result"], "done");
                        }
                    }
                    serde_json::json!({"role":"assistant","content":"done"})
                };
                request.respond(tiny_http::Response::from_string(serde_json::json!({
                    "choices":[{"message":message,"finish_reason":if first || join {"tool_calls"} else {"stop"}}]
                }).to_string()).with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap())).unwrap();
            }));
        }
        for handler in handlers {
            handler.join().unwrap();
        }
    });
    let config = serde_json::json!({"name":"lead","instructions":"delegate then finish","model":"root-model","endpoint":endpoint,
    "shared_model_budget":{"group":"team-test","limit":10},
    "subagents":[
        {"name":"a","instructions":"finish","model":"a-model","endpoint":endpoint},
        {"name":"b","instructions":"finish","model":"b-model","endpoint":endpoint},
        {"name":"c","instructions":"use prerequisites and finish","model":"c-model","endpoint":endpoint}
    ]});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), config.to_string()).unwrap();
    let actor = Actor {
        workspace_id: "local".into(),
        id: "developer".into(),
    };
    let store = Store::default();
    let store_guard = store.clone();
    let config = Configuration::load(file.path()).unwrap();
    if published {
        store
            .publish_configuration(&actor, &config, "team")
            .unwrap();
    }
    let target = hudson_temporal::ExecutionClient::schedule_target("test", "hudson-team-test");
    let tree = config.build_temporal_tree(store.clone(), &actor).unwrap();
    let id = tree
        .runtime
        .submit_scheduled(
            &actor,
            tree.reference.clone(),
            serde_json::json!("work"),
            None,
            None,
            Some(target.clone()),
        )
        .unwrap();
    drop(file);
    let inspect_actor = actor.clone();
    let activities = if published {
        drop(tree);
        RunActivities::published(store.clone(), actor, target).unwrap()
    } else {
        RunActivities::new(tree.into_scheduled(), actor)
    };
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            let env = WorkflowEnvironment::start_local(LocalWorkflowEnvironmentOptions::default())
                .await
                .unwrap();
            let runtime = Runtime::from_current_tokio(Default::default()).unwrap();
            let options = WorkerOptions::new("hudson-team-test")
                .register_workflow::<RunWorkflow>()
                .unwrap()
                .register_activities(activities)
                .build();
            let mut worker = Worker::new(&runtime, env.client().clone(), options).unwrap();
            let stop = worker.shutdown_handle();
            let handle = env
                .client()
                .start_workflow(
                    RunWorkflow::run,
                    id,
                    WorkflowStartOptions::new("hudson-team-test", format!("test-{id}")).build(),
                )
                .await
                .unwrap();
            let (worker_result, result) = tokio::join!(worker.run(), async {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(50),
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
    assert_eq!(children_done.load(Ordering::SeqCst), 3);
    let children = store.children(&inspect_actor, id).unwrap();
    assert_eq!(children.len(), 3);
    assert!(children
        .iter()
        .all(|child| child.status == RunStatus::Completed));
    server_thread.join().unwrap();
    drop(store_guard);
}

#[test]
#[ignore = "requires local PostgreSQL hudson_harness_test_20260921 and Temporal CLI"]
fn question_survives_worker_restart_with_postgres() {
    question_scenario(false);
}

#[test]
#[ignore = "requires PostgreSQL and a local Temporal server"]
fn published_question_survives_worker_restart_with_postgres() {
    question_scenario(true);
}

fn question_scenario(published: bool) {
    use hudson_core::models::WaitReason;
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/chat", server.server_addr());
    let server_thread = std::thread::spawn(move || {
        for index in 0..2 {
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(50))
                .unwrap()
                .expect("missing model request");
            let body: serde_json::Value = serde_json::from_reader(request.as_reader()).unwrap();
            let message = if index == 0 {
                serde_json::json!({"role":"assistant","content":null,"tool_calls":[{"id":"question","type":"function","function":{"name":"ask_user","arguments":"{\"prompt\":\"Which city?\"}"}}]})
            } else {
                assert!(body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["role"] == "tool"
                        && m["tool_call_id"] == "question"
                        && m["content"].as_str().unwrap().contains("Boston")));
                serde_json::json!({"role":"assistant","content":"Boston selected"})
            };
            request.respond(tiny_http::Response::from_string(serde_json::json!({"choices":[{"message":message,"finish_reason":if index == 0 {"tool_calls"} else {"stop"}}]}).to_string())).unwrap();
        }
    });
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), serde_json::json!({"name":"question-agent","instructions":"ask then finish","allow_user_input":true,"endpoint":endpoint}).to_string()).unwrap();
    let namespace = format!("temporal-restart-{}", uuid::Uuid::new_v4());
    let database = std::env::var("HUDSON_TEST_DATABASE")
        .unwrap_or_else(|_| "hudson_harness_test_20260921".into());
    let actor = Actor {
        workspace_id: "local".into(),
        id: "developer".into(),
    };
    let store = Store::postgres_local("/tmp", &database, &namespace).unwrap();
    let store_guard = store.clone();
    let config = Configuration::load(file.path()).unwrap();
    if published {
        store
            .publish_configuration(&actor, &config, "question")
            .unwrap();
        std::fs::remove_file(file.path()).unwrap();
    }
    let target = hudson_temporal::ExecutionClient::schedule_target(&namespace, "hudson-restart");
    let tree = config.build_temporal_tree(store.clone(), &actor).unwrap();
    let id = tree
        .runtime
        .submit_scheduled(
            &actor,
            tree.reference.clone(),
            serde_json::json!("choose a city"),
            Some("restart-test".into()),
            None,
            Some(target.clone()),
        )
        .unwrap();
    let activities = if published {
        drop(tree);
        RunActivities::published(store.clone(), actor.clone(), target.clone()).unwrap()
    } else {
        RunActivities::new(tree.into_scheduled(), actor.clone())
    };
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            let env = WorkflowEnvironment::start_local(LocalWorkflowEnvironmentOptions::default())
                .await
                .unwrap();
            let runtime = Runtime::from_current_tokio(Default::default()).unwrap();
            let options = WorkerOptions::new("hudson-restart")
                .register_workflow::<RunWorkflow>()
                .unwrap()
                .register_activities(activities)
                .build();
            let mut worker = Worker::new(&runtime, env.client().clone(), options).unwrap();
            let stop = worker.shutdown_handle();
            let handle = env
                .client()
                .start_workflow(
                    RunWorkflow::run,
                    id,
                    WorkflowStartOptions::new("hudson-restart", format!("test-{id}")).build(),
                )
                .await
                .unwrap();
            let (worked, question) = tokio::join!(worker.run(), async {
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
                loop {
                    let store = store.clone();
                    let actor = actor.clone();
                    let view =
                        tokio::task::spawn_blocking(move || store.inspect(&actor, id).unwrap())
                            .await
                            .unwrap();
                    if let Some(WaitReason::UserInput { question_id, .. }) = view.wait {
                        stop();
                        break question_id;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        stop();
                        panic!("no question");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            });
            worked.unwrap();
            drop(worker);
            // Reopen PostgreSQL and rebuild every model/tool executor, then apply
            // the answer while no Temporal worker is running.
            let actor2 = actor.clone();
            let activities = tokio::task::spawn_blocking(move || {
                let store = Store::postgres_local("/tmp", &database, &namespace).unwrap();
                let config = if published {
                    let root = store.published_root(&actor2, id).unwrap();
                    store.published_configuration(&actor2, &root).unwrap()
                } else {
                    Configuration::load(file.path()).unwrap()
                };
                let tree = config.build_admission_tree(store.clone(), &actor2).unwrap();
                tree.runtime
                    .provide_input(
                        &actor2,
                        id,
                        question,
                        "answer-1",
                        serde_json::json!("Boston"),
                    )
                    .unwrap();
                drop(tree);
                if published {
                    RunActivities::published(store, actor2, target).unwrap()
                } else {
                    let tree = Configuration::load(file.path())
                        .unwrap()
                        .build_temporal_tree(store, &actor2)
                        .unwrap();
                    RunActivities::new(tree.into_scheduled(), actor2)
                }
            })
            .await
            .unwrap();
            let options = WorkerOptions::new("hudson-restart")
                .register_workflow::<RunWorkflow>()
                .unwrap()
                .register_activities(activities)
                .build();
            let mut worker = Worker::new(&runtime, env.client().clone(), options).unwrap();
            let stop = worker.shutdown_handle();
            let (worked, result) = tokio::join!(worker.run(), async {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    handle.get_result(WorkflowGetResultOptions::default()),
                )
                .await;
                stop();
                result
            });
            worked.unwrap();
            env.shutdown().await.unwrap();
            assert_eq!(result.unwrap().unwrap().status, RunStatus::Completed);
        });
    server_thread.join().unwrap();
    drop(store_guard);
}

#[test]
#[ignore = "requires local PostgreSQL and starts separate Temporal worker/client processes"]
fn background_returns_without_worker_and_foreground_waits_for_same_run() {
    cli_scenario(SubmissionMode::Direct);
}

#[test]
#[ignore = "requires local PostgreSQL and separate Temporal worker/client processes"]
fn worker_recovers_submission_without_a_temporal_start() {
    cli_scenario(SubmissionMode::FailedClient);
}

#[test]
#[ignore = "requires built hudson-server, PostgreSQL and a local Temporal server"]
fn api_submission_survives_api_exit_and_runs_on_separate_worker() {
    cli_scenario(SubmissionMode::Http);
}

#[test]
#[ignore = "requires PostgreSQL and separate Temporal worker processes"]
fn published_worker_executes_without_configuration_file() {
    cli_scenario(SubmissionMode::Published);
}

enum SubmissionMode {
    Published,
    Direct,
    FailedClient,
    Http,
}

fn cli_scenario(mode: SubmissionMode) {
    use std::process::{Child, Command, Stdio};
    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn output(mut child: Child) -> std::process::Output {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("CLI timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        child.wait_with_output().unwrap()
    }
    let port_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = port_listener.local_addr().unwrap().port();
    drop(port_listener);
    tokio::runtime::Runtime::new().unwrap().block_on(async move {
        let env = WorkflowEnvironment::start_local(LocalWorkflowEnvironmentOptions::builder().port(port).build()).await.unwrap();
        tokio::task::spawn_blocking(move || {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}/chat", server.server_addr());
            let server_thread = std::thread::spawn(move || {
                let mut request = server.recv_timeout(std::time::Duration::from_secs(30)).unwrap().expect("no model call");
                let body: serde_json::Value = serde_json::from_reader(request.as_reader()).unwrap();
                let user = body["messages"].as_array().unwrap().iter().find(|m| m["role"] == "user").unwrap();
                assert_eq!(serde_json::from_str::<serde_json::Value>(user["content"].as_str().unwrap()).unwrap(), serde_json::json!({"task":"finish"}));
                request.respond(tiny_http::Response::from_string(r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#)).unwrap();
            });
            let file = tempfile::NamedTempFile::new().unwrap();
            let mut definition = serde_json::json!({"name":"cli-agent","instructions":"finish","endpoint":endpoint,"input_schema":{"type":"object","required":["task"]}});
            if matches!(mode, SubmissionMode::Http) {
                definition["api_key_env"] = serde_json::json!("HUDSON_EXECUTION_ONLY_TEST_KEY");
                definition["http_tools"] = serde_json::json!([{"name":"customer_lookup", "description":"lookup",
                    "endpoint":endpoint, "token_env":"HUDSON_EXECUTION_ONLY_TEST_KEY",
                    "input_schema":{"type":"object"}, "effect":"read"}]);
            }
            std::fs::write(file.path(), definition.to_string()).unwrap();
            let input = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(input.path(), r#"{"task":"finish"}"#).unwrap();
            let database = std::env::var("HUDSON_TEST_DATABASE").unwrap_or_else(|_| "hudson_harness_test_20260921".into());
            let namespace = format!("temporal-cli-{}", uuid::Uuid::new_v4());
            let command = || {
                let mut command = Command::new(env!("CARGO_BIN_EXE_hudson-temporal"));
                if matches!(mode, SubmissionMode::Published) {
                    command.arg("--published");
                } else {
                    command.args(["--config", file.path().to_str().unwrap()]);
                }
                command.args(["--database", &database, "--namespace", &namespace, "--task-queue", &namespace])
                    .env("TEMPORAL_ADDRESS", format!("127.0.0.1:{port}"))
                    .env("TEMPORAL_NAMESPACE", "default")
                    .env("HUDSON_EXECUTION_ONLY_TEST_KEY", "test-worker-only")
                    .stdout(Stdio::piped()).stderr(Stdio::piped());
                command
            };
            if !matches!(mode, SubmissionMode::Direct) {
                let store = Store::postgres_local("/tmp", &database, &namespace).unwrap();
                let actor = Actor { workspace_id:"local".into(), id:"developer".into() };
                let config = hudson_core::configured::Configuration::load(file.path()).unwrap();
                if matches!(mode, SubmissionMode::Published) {
                    store.publish_configuration(&actor, &config, "publication").unwrap();
                }
                let tree = config.build_admission_tree(store.clone(), &actor).unwrap();
                let incompatible = tree.runtime.submit_scheduled(
                    &actor, tree.reference.clone(), serde_json::json!({"task":"finish"}),
                    Some("incompatible-goal".into()),
                    Some(hudson_core::models::Goal {
                        objective: "A different immutable goal".into(),
                        success_schema: serde_json::json!({"type":"string"}),
                        criteria: vec![],
                    }),
                    Some(hudson_temporal::ExecutionClient::schedule_target(&namespace, &namespace)),
                ).unwrap();
                drop(tree);
                let id: uuid::Uuid = if matches!(mode, SubmissionMode::Http) {
                    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                    let api_port = socket.local_addr().unwrap().port();
                    drop(socket);
                    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_hudson-temporal")).with_file_name("hudson-server");
                    assert!(binary.exists(), "build hudson-server before this integration test");
                    let api = Process(Command::new(binary).args([
                        "--config", file.path().to_str().unwrap(), "--database", &database,
                        "--namespace", &namespace, "--temporal-task-queue", &namespace,
                        "--port", &api_port.to_string(),
                    ]).env_remove("HUDSON_EXECUTION_ONLY_TEST_KEY").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap());
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while std::net::TcpStream::connect(("127.0.0.1", api_port)).is_err() {
                        assert!(std::time::Instant::now() < deadline, "API did not start");
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    let id = tokio::runtime::Runtime::new().unwrap().block_on(async {
                        let client = reqwest::Client::new();
                        let mut receipts = vec![];
                        for _ in 0..2 {
                            let response = client.post(format!("http://127.0.0.1:{api_port}/runs"))
                                .header("content-type", "application/json")
                                .body(r#"{"input":{"task":"finish"},"request_key":"api-task"}"#)
                                .send().await.unwrap();
                            assert_eq!(response.status().as_u16(), 202);
                            receipts.push(serde_json::from_str::<serde_json::Value>(&response.text().await.unwrap()).unwrap());
                        }
                        assert_eq!(receipts[0], receipts[1]);
                        receipts[0]["run_id"].as_str().unwrap().parse().unwrap()
                    });
                    drop(api); // No API process or client remains when the worker starts.
                    id
                } else if matches!(mode, SubmissionMode::Published) {
                    let config = store.published_configuration(&actor, &store.published_root(&actor, incompatible).unwrap()).unwrap();
                    let tree = config.build_admission_tree(store.clone(), &actor).unwrap();
                    let id = tree.runtime.submit_scheduled(&actor, tree.reference.clone(), serde_json::json!({"task":"finish"}),
                        Some("published-run".into()), tree.goal.clone(),
                        Some(hudson_temporal::ExecutionClient::schedule_target(&namespace, &namespace))).unwrap();
                    std::fs::remove_file(file.path()).unwrap();
                    id
                } else {
                let interrupted = output(command().env("TEMPORAL_ADDRESS", "http://[invalid").args(["run","--input-file",input.path().to_str().unwrap(),"--request-key","task-1","--background"]).spawn().unwrap());
                assert!(!interrupted.status.success());
                let stderr = String::from_utf8(interrupted.stderr).unwrap();
                let failed_id: uuid::Uuid = stderr.lines().find_map(|line| line.strip_prefix("Run: ")).expect("run was persisted before connection failure").parse().unwrap();
                    failed_id
                };
                let store = Store::postgres_local("/tmp", &database, &namespace).unwrap();
                let actor = Actor { workspace_id:"local".into(), id:"developer".into() };
                assert_eq!(store.inspect(&actor, id).unwrap().status, RunStatus::Queued);
                let worker = Process(command().arg("worker").spawn().unwrap());
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                loop {
                    let view = store.inspect(&actor, id).unwrap();
                    if view.status.terminal() { assert_eq!(view.status, RunStatus::Completed); break; }
                    assert!(std::time::Instant::now() < deadline, "worker did not publish saved intent");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                server_thread.join().unwrap();
                assert_eq!(store.inspect(&actor, incompatible).unwrap().status, RunStatus::Queued,
                    "a mismatched goal must remain deferred while valid work completes");
                if matches!(mode, SubmissionMode::Published) {
                    drop(worker);
                    return;
                }
                // PostgreSQL completion precedes Temporal's workflow result.
                // Keep the worker alive until Temporal records that result,
                // then verify a completed run can be read without a worker.
                let finished = output(command().args(["run","--resume", &id.to_string()]).spawn().unwrap());
                assert!(finished.status.success(), "{}", String::from_utf8_lossy(&finished.stderr));
                drop(worker);
                let completed = output(command().args(["run","--resume", &id.to_string()]).spawn().unwrap());
                assert!(completed.status.success(), "{}", String::from_utf8_lossy(&completed.stderr));
                return;
            }
            let background = output(command().args(["run","--input-file",input.path().to_str().unwrap(),"--request-key","task-1","--background"]).spawn().unwrap());
            assert!(background.status.success(), "{}", String::from_utf8_lossy(&background.stderr));
            let receipt: serde_json::Value = serde_json::from_slice(&background.stdout).unwrap();
            let id = receipt["run_id"].as_str().unwrap();
            let repeated = output(command().args(["run","--input-file",input.path().to_str().unwrap(),"--request-key","task-1","--background"]).spawn().unwrap());
            assert!(repeated.status.success(), "{}", String::from_utf8_lossy(&repeated.stderr));
            assert_eq!(serde_json::from_slice::<serde_json::Value>(&repeated.stdout).unwrap()["run_id"], id);
            let mut foreground = command().args(["run","--resume",id]).spawn().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(500));
            assert!(foreground.try_wait().unwrap().is_none(), "foreground returned before a worker existed");
            let worker = Process(command().arg("worker").spawn().unwrap());
            let foreground = output(foreground);
            assert!(foreground.status.success(), "{}", String::from_utf8_lossy(&foreground.stderr));
            let stdout = String::from_utf8(foreground.stdout).unwrap();
            assert!(stdout.contains(id));
            assert!(stdout.contains("\"status\": \"completed\""));
            server_thread.join().unwrap();
            drop(worker);
            let completed = output(command().args(["run","--resume",id]).spawn().unwrap());
            assert!(completed.status.success(), "{}", String::from_utf8_lossy(&completed.stderr));
            assert!(String::from_utf8(completed.stdout).unwrap().contains("\"status\": \"completed\""));
        }).await.unwrap();
        env.shutdown().await.unwrap();
    });
}
