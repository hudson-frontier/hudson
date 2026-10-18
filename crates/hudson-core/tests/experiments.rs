use hudson_core::{
    configured::Configuration,
    experiments::{Driver, Spec, Status, View},
    inbox::MessageKind,
    models::*,
    storage::Store,
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
enum Mode {
    Improve,
    Tie,
    Regress,
    Invalid,
    BadLesson,
    Unknown,
}
struct Fixture {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/model", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                let request = read_request(&mut socket);
                observed.lock().unwrap().push(request.clone());
                if matches!(mode, Mode::Unknown) {
                    continue;
                }
                let response = response(&request, mode);
                let body = json!({"choices":[response],"usage":{"prompt_tokens":10,"completion_tokens":5}}).to_string();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            endpoint,
            requests,
            stop,
            thread: Some(thread),
        }
    }
    fn configuration(&self) -> Configuration {
        configuration(self.template())
    }
    fn template(&self) -> Value {
        json!({"name":"baseline","model":"fixture","endpoint":self.endpoint,"instructions":"BASE",
            "skills":[{"name":"procedure","description":"Fixed description","instructions":"BASE_SKILL"}],
            "input_schema":{"type":"string"},"output_schema":{"type":"object","required":["answer"],"properties":{"answer":{"type":"integer"}}}})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn read_request(socket: &mut TcpStream) -> Value {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 8192);
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let headers = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
    assert!(!headers.contains("authorization:"));
    let length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .unwrap()
        .parse()
        .unwrap();
    assert!(length <= 128 * 1024);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}
fn response(request: &Value, mode: Mode) -> Value {
    assert!(!request.to_string().contains("PRIVATE_GRADING_CRITERIA"));
    let instructions = request["messages"][0]["content"].as_str().unwrap();
    let user = request["messages"][1]["content"].as_str().unwrap();
    let stage = instructions.starts_with("Offline harness experiment stage:");
    let final_output =
        |value: Value| json!({"finish_reason":"stop","message":{"content":value.to_string()}});
    if stage {
        assert!(request.get("tools").is_none());
        assert!(!request.to_string().contains("PRIVATE_GRADING_CRITERIA"));
        assert!(!user.contains("hidden-"));
        let input: Value = serde_json::from_str(user).unwrap();
        let role = input["stage"].as_str().unwrap();
        if role == "reflect" {
            let ids = if matches!(mode, Mode::BadLesson) {
                vec![json!(uuid::Uuid::new_v4())]
            } else {
                input["probe_executions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|run| run["run_id"].clone())
                    .collect()
            };
            return final_output(
                json!({"cue":"fixture tasks","lesson":"Read the frozen procedure before answering; the listed executions show the resulting behavior.","run_ids":ids}),
            );
        }
        let label = match role {
            "general" => "GENERAL",
            "specific" => "SPECIFIC",
            "compose" => "COMPOSED",
            _ => panic!("unknown role"),
        };
        let mut variant =
            json!({"instructions":label,"skills":{"procedure":format!("{label}_SKILL")}});
        if matches!(mode, Mode::Invalid) {
            variant["endpoint"] = json!("https://forbidden.invalid");
        }
        return final_output(variant);
    }
    let has_result = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool");
    if !has_result {
        return json!({"finish_reason":"tool_calls","message":{"tool_calls":[{"id":"skill","type":"function","function":{"name":"load_skill","arguments":"{\"name\":\"procedure\"}"}}]}});
    }
    let last_tool = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    assert!(last_tool["content"]
        .as_str()
        .unwrap()
        .contains(&format!("{instructions}_SKILL")));
    let composed = instructions == "COMPOSED";
    let answer = match user {
        "hidden-a" if matches!(mode, Mode::Tie) || composed != matches!(mode, Mode::Regress) => 42,
        "hidden-a" => 0,
        "hidden-b" if matches!(mode, Mode::Regress) && !composed => 0,
        "hidden-b" => 7,
        "hidden-c" if matches!(mode, Mode::Regress) && !composed => 0,
        "hidden-c" => 9,
        _ => 1,
    };
    final_output(json!({"answer":answer}))
}

#[test]
fn lesson_snapshots_do_not_read_future_adoptions_and_cancel_wins_before_selection() {
    let fixture = Fixture::new(Mode::Improve);
    let store = Store::default();
    let config = fixture.configuration();
    let mut first = Driver::open(store.clone(), actor(), config.clone(), spec("first")).unwrap();
    let early = Driver::open(
        store.clone(),
        actor(),
        config.clone(),
        spec("already-started"),
    )
    .unwrap();
    assert!(early.inspect().unwrap().lesson_snapshot.is_empty());
    assert!(drive(&mut first).lesson_id.is_some());
    let early = Driver::open(
        store.clone(),
        actor(),
        config.clone(),
        spec("already-started"),
    )
    .unwrap();
    assert!(early.inspect().unwrap().lesson_snapshot.is_empty());
    let mut cancelled = Driver::open(
        store.clone(),
        actor(),
        config,
        spec("cancel-before-selection"),
    )
    .unwrap();
    for _ in 0..4096 {
        let view = cancelled.tick().unwrap();
        if view.stages.len() == 14 && view.stages.last().unwrap().run.status == RunStatus::Completed
        {
            cancelled.cancel().unwrap();
            let final_view = cancelled.tick().unwrap();
            assert_eq!(final_view.status, Status::Cancelled);
            assert_eq!(final_view.selected.version, 1);
            assert!(final_view.lesson_id.is_none());
            return;
        }
    }
    panic!("fixture tick ceiling")
}

/// Identical controlled workload in both modes; no provider credentials or database.
#[test]
#[ignore = "manual resource/latency workload; set HUDSON_EXPERIMENT_RELEASE_EACH_TICK for comparison"]
fn fixture_experiment_workload() {
    let fixture = Fixture::new(Mode::Improve);
    let release = std::env::var_os("HUDSON_EXPERIMENT_RELEASE_EACH_TICK").is_some();
    let mut samples = Vec::with_capacity(5);
    for index in 0..5 {
        let mut driver = Driver::open(
            Store::default(),
            actor(),
            fixture.configuration(),
            spec(&format!("workload-{index}")),
        )
        .unwrap();
        let started = Instant::now();
        for _ in 0..4096 {
            let view = driver.tick().unwrap();
            if release {
                driver.release_resources();
            }
            if view.status.terminal() {
                assert_eq!(view.status, Status::Completed);
                assert_eq!(view.selected.version, 4);
                assert_eq!(view.admitted_model_calls, 24);
                samples.push(started.elapsed().as_micros());
                break;
            }
            assert!(
                !matches!(view.status, Status::Waiting | Status::Cancelling),
                "fixture unexpectedly blocked"
            );
        }
    }
    assert_eq!(samples.len(), 5);
    assert_eq!(fixture.requests.lock().unwrap().len(), 120);
    println!("experiment_workload release_each_tick={release} samples_us={samples:?}");
}
fn configuration(value: Value) -> Configuration {
    try_configuration(value).unwrap()
}
fn try_configuration(value: Value) -> Result<Configuration, Box<dyn std::error::Error>> {
    let path =
        std::env::temp_dir().join(format!("hudson-experiment-{}.json", uuid::Uuid::new_v4()));
    std::fs::write(&path, serde_json::to_vec(&value)?)?;
    let configuration = Configuration::load(&path);
    std::fs::remove_file(&path)?;
    configuration
}
fn actor() -> Actor {
    Actor {
        workspace_id: "experiment-tests".into(),
        id: "operator".into(),
    }
}
fn spec(key: &str) -> Spec {
    serde_json::from_value(json!({"request_key":key,"domain":"fixture-domain","probe":"visible-probe","max_model_calls":64,
        "validation":[
            {"name":"case-a","input":"hidden-a","expected_schema":{"description":"PRIVATE_GRADING_CRITERIA","type":"object","properties":{"answer":{"const":42}},"required":["answer"]}},
            {"name":"case-b","input":"hidden-b","expected_schema":{"type":"object","properties":{"answer":{"const":7}},"required":["answer"]}},
            {"name":"case-c","input":"hidden-c","expected_schema":{"type":"object","properties":{"answer":{"const":9}},"required":["answer"]}}
        ]})).unwrap()
}
fn drive(driver: &mut Driver) -> View {
    for _ in 0..4096 {
        let view = driver.tick().unwrap();
        if view.status.terminal() || matches!(view.status, Status::Waiting | Status::Cancelling) {
            return view;
        }
    }
    panic!("fixture tick ceiling")
}

#[test]
fn composition_improves_held_out_results_with_fixed_skills_and_one_budget() {
    let fixture = Fixture::new(Mode::Improve);
    let store = Store::default();
    let config = fixture.configuration();
    let mut driver =
        Driver::open(store.clone(), actor(), config.clone(), spec("improvement")).unwrap();
    let view = drive(&mut driver);
    assert_eq!(view.status, Status::Completed);
    assert_eq!(view.selected.version, 4);
    assert_eq!(
        view.candidate.unwrap().skills["procedure"],
        "COMPOSED_SKILL"
    );
    assert_eq!(
        view.comparisons
            .iter()
            .map(|c| c.baseline_passed)
            .collect::<Vec<_>>(),
        [Some(false), Some(true), Some(true)]
    );
    assert!(view
        .comparisons
        .iter()
        .all(|case| case.candidate_passed == Some(true)));
    assert_eq!(view.stages.len(), 14);
    assert_eq!(view.admitted_model_calls, 24); // Ten two-call solver runs and four one-call editing runs.
    assert_eq!(fixture.requests.lock().unwrap().len(), 24);
    assert!(view.lesson_id.is_some());
    assert!(view.lesson_snapshot.is_empty());
    let before: Vec<_> = view.stages.iter().map(|s| s.run.id).collect();
    drop(driver);
    let mut resumed =
        Driver::open(store.clone(), actor(), config.clone(), spec("improvement")).unwrap();
    let replay = resumed.tick().unwrap();
    assert_eq!(
        before,
        replay.stages.iter().map(|s| s.run.id).collect::<Vec<_>>()
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 24);
    let next = Driver::open(store.clone(), actor(), config.clone(), spec("next-task"))
        .unwrap()
        .inspect()
        .unwrap();
    assert_eq!(next.lesson_snapshot, [view.lesson_id.unwrap()]);
    let mut other_domain = spec("other-domain");
    other_domain.domain = "another".into();
    assert!(
        Driver::open(store.clone(), actor(), config.clone(), other_domain)
            .unwrap()
            .inspect()
            .unwrap()
            .lesson_snapshot
            .is_empty()
    );
    let mut changed = spec("improvement");
    changed.max_model_calls += 1;
    assert!(Driver::open(store.clone(), actor(), config, changed).is_err());
    let other = Actor {
        id: "another-actor".into(),
        ..actor()
    };
    assert!(store.inspect_experiment(&other, view.id).is_err());
    assert!(store.cancel_experiment(&other, view.id).is_err());
}

#[test]
fn ties_and_quality_regressions_retain_the_baseline() {
    for mode in [Mode::Tie, Mode::Regress] {
        let fixture = Fixture::new(mode);
        let mut driver = Driver::open(
            Store::default(),
            actor(),
            fixture.configuration(),
            spec("baseline-fallback"),
        )
        .unwrap();
        let view = drive(&mut driver);
        assert_eq!(view.status, Status::Completed);
        assert_eq!(view.selected.version, 1);
        assert!(view.lesson_id.is_none());
        assert!(view.candidate.is_none());
        if matches!(mode, Mode::Regress) {
            assert_eq!(
                view.comparisons
                    .iter()
                    .filter(|c| c.baseline_passed == Some(true))
                    .count(),
                1
            );
            assert_eq!(
                view.comparisons
                    .iter()
                    .filter(|c| c.candidate_passed == Some(true))
                    .count(),
                2
            );
        }
    }
}

#[test]
fn malformed_proposals_and_unverifiable_lessons_cannot_be_adopted() {
    for mode in [Mode::Invalid, Mode::BadLesson] {
        let fixture = Fixture::new(mode);
        let mut driver = Driver::open(
            Store::default(),
            actor(),
            fixture.configuration(),
            spec("invalid-output"),
        )
        .unwrap();
        let view = drive(&mut driver);
        assert_eq!(view.status, Status::Failed);
        assert_eq!(view.selected.version, 1);
        assert!(view.comparisons.is_empty());
        assert!(view.lesson_id.is_none());
        assert!(view.admitted_model_calls <= 14);
    }
}

#[test]
fn exhausted_global_budget_survives_reopen_without_hidden_requests() {
    let fixture = Fixture::new(Mode::Improve);
    let store = Store::default();
    let config = fixture.configuration();
    let mut limited = spec("bounded");
    limited.max_model_calls = 4;
    let mut driver = Driver::open(store.clone(), actor(), config.clone(), limited.clone()).unwrap();
    let view = drive(&mut driver);
    assert_eq!(view.status, Status::Failed);
    assert_eq!(view.admitted_model_calls, 4);
    assert_eq!(fixture.requests.lock().unwrap().len(), 4);
    let mut resumed = Driver::open(store, actor(), config, limited).unwrap();
    assert_eq!(resumed.tick().unwrap().admitted_model_calls, 4);
    assert_eq!(fixture.requests.lock().unwrap().len(), 4);
}

#[test]
fn cancellation_fences_pending_stages_and_external_input_is_rejected() {
    let fixture = Fixture::new(Mode::Improve);
    let store = Store::default();
    let mut driver = Driver::open(
        store.clone(),
        actor(),
        fixture.configuration(),
        spec("cancel"),
    )
    .unwrap();
    let first = driver.tick().unwrap();
    let run_id = first.stages[0].run.id;
    assert!(store
        .enqueue_message(
            &actor(),
            run_id,
            MessageKind::Steer,
            json!("PRIVATE_GRADING_CRITERIA"),
            "tamper"
        )
        .is_err());
    driver.cancel().unwrap();
    let view = driver.tick().unwrap();
    assert_eq!(view.status, Status::Cancelled);
    assert_eq!(view.admitted_model_calls, 0);
    assert!(fixture.requests.lock().unwrap().is_empty());
    assert_eq!(view.stages[0].run.status, RunStatus::Cancelled);
}

#[test]
fn ordinary_admission_cannot_substitute_for_a_fixture_execution_binding() {
    let fixture = Fixture::new(Mode::Improve);
    let store = Store::default();
    let specification = spec("collision");
    let mut driver = Driver::open(
        store.clone(),
        actor(),
        fixture.configuration(),
        specification.clone(),
    )
    .unwrap();
    let mut template = fixture.template();
    template["name"] = json!(driver.inspect().unwrap().selected.id);
    let group = format!(
        "experiment-{}",
        hudson_core::definitions::digest(&(
            "offline-experiment-v1",
            actor(),
            &specification.request_key
        ))
        .unwrap()
    );
    template["shared_model_budget"] = json!({"group":group,"limit":specification.max_model_calls});
    let tree = configuration(template)
        .build_admission_tree(store.clone(), &actor())
        .unwrap();
    let foreign = tree
        .runtime
        .submit(
            &actor(),
            tree.reference,
            specification.probe,
            Some(format!("experiment:{}:0", driver.id())),
        )
        .unwrap();
    assert!(
        matches!(driver.tick(), Err(hudson_core::Error::Conflict(message)) if message.contains("transport"))
    );
    assert!(driver.inspect().unwrap().stages.is_empty());
    driver.cancel().unwrap();
    assert_eq!(driver.tick().unwrap().status, Status::Cancelled);
    assert_eq!(
        store.inspect(&actor(), foreign).unwrap().status,
        RunStatus::Queued
    );
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[test]
fn uncertain_model_remains_blocked_and_cancellation_does_not_replay_it() {
    let fixture = Fixture::new(Mode::Unknown);
    let store = Store::default();
    let mut driver =
        Driver::open(store, actor(), fixture.configuration(), spec("uncertain")).unwrap();
    let waiting = drive(&mut driver);
    assert_eq!(waiting.status, Status::Waiting);
    assert_eq!(waiting.admitted_model_calls, 1);
    assert!(matches!(
        waiting.stages[0].run.wait,
        Some(WaitReason::Reconciliation { .. })
    ));
    for _ in 0..3 {
        assert_eq!(driver.tick().unwrap().status, Status::Waiting);
    }
    driver.cancel().unwrap();
    assert_eq!(driver.tick().unwrap().status, Status::Cancelling);
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}

#[test]
fn plans_reject_control_changes_nonlocal_endpoints_and_probe_leakage_before_io() {
    let fixture = Fixture::new(Mode::Improve);
    let baseline = fixture.template();
    for (key, value) in [
        ("endpoint", json!("https://provider.invalid/model")),
        ("api_key_env", json!("SECRET")),
        (
            "shared_model_budget",
            json!({"group":"existing","limit":100}),
        ),
        ("allow_user_input", json!(true)),
        (
            "memory",
            json!({"scope":{"name":"shared"},"recall_limit":4}),
        ),
    ] {
        let mut changed = baseline.clone();
        changed[key] = value;
        // Some invalid templates fail the ordinary configuration validator first.
        if let Ok(config) = try_configuration(changed) {
            assert!(Driver::plan(&config, &spec("plan")).is_err());
        }
    }
    let mut duplicate = spec("duplicate");
    duplicate.probe = duplicate.validation[0].input.clone();
    assert!(Driver::plan(&fixture.configuration(), &duplicate).is_err());
    let plan = Driver::plan(&fixture.configuration(), &spec("valid-plan")).unwrap();
    assert_eq!(plan.refinement_steps, 1);
    assert_eq!(plan.probe_runs, 4);
    assert_eq!(plan.editing_runs, 4);
    assert_eq!(plan.validation_runs, 6);
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[test]
fn concurrent_drivers_share_each_stage_and_never_duplicate_provider_calls() {
    let fixture = Fixture::new(Mode::Improve);
    let store = Store::default();
    let config = fixture.configuration();
    let id = Driver::open(store.clone(), actor(), config.clone(), spec("concurrent"))
        .unwrap()
        .id();
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let store = store.clone();
            scope.spawn(move || {
                let mut driver = Driver::resume(store, actor(), id).unwrap();
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(60) {
                    match driver.tick() {
                        Ok(view) if view.status.terminal() => {
                            assert_eq!(view.status, Status::Completed);
                            return;
                        }
                        Ok(_) | Err(hudson_core::Error::Conflict(_)) => {
                            std::thread::sleep(Duration::from_millis(1))
                        }
                        Err(error) => panic!("driver failed: {error}"),
                    }
                }
                panic!("concurrent driver deadline")
            });
        }
    });
    let view = store.inspect_experiment(&actor(), id).unwrap();
    assert_eq!(view.admitted_model_calls, 24);
    assert_eq!(fixture.requests.lock().unwrap().len(), 24);
    assert!(view.lesson_id.is_some());
}

#[test]
#[ignore = "requires HUDSON_TEST_DATABASE in local PostgreSQL"]
fn postgres_reconnect_preserves_stages_fingerprints_budget_and_format() {
    let fixture = Fixture::new(Mode::Improve);
    let database = std::env::var("HUDSON_TEST_DATABASE").unwrap();
    let namespace = format!("experiments-{}", uuid::Uuid::new_v4());
    let connect = || Store::postgres_local("/tmp", &database, &namespace).unwrap();
    let config = fixture.configuration();
    let mut driver = Driver::open(connect(), actor(), config.clone(), spec("durable")).unwrap();
    let id = driver.id();
    loop {
        let view = driver.tick().unwrap();
        if view.stages.len() >= 4 {
            break;
        }
    }
    let before = driver.inspect().unwrap();
    drop(driver);
    let mut resumed = Driver::open(connect(), actor(), config.clone(), spec("durable")).unwrap();
    assert_eq!(resumed.id(), id);
    assert_eq!(
        before.manifest_digest,
        resumed.inspect().unwrap().manifest_digest
    );
    let after = drive(&mut resumed);
    assert_eq!(after.status, Status::Completed);
    assert_eq!(after.admitted_model_calls, 24);
    assert_eq!(fixture.requests.lock().unwrap().len(), 24);
    drop(resumed);
    let mut client = postgres::Config::new()
        .host("/tmp")
        .dbname(&database)
        .connect(postgres::NoTls)
        .unwrap();
    let row = client
        .query_one(
            "SELECT schema_version,state FROM hudson_state WHERE namespace=$1",
            &[&namespace],
        )
        .unwrap();
    assert_eq!(row.get::<_, i32>(0), 3);
    let data: Value = row.get(1);
    assert_eq!(data["experiments"].as_object().unwrap().len(), 1);
    let changed = configuration({
        let mut v = fixture.template();
        v["instructions"] = json!("CHANGED");
        v
    });
    assert!(Driver::open(connect(), actor(), changed, spec("durable")).is_err());
    assert_eq!(fixture.requests.lock().unwrap().len(), 24);
    let experiment_key = id.to_string();
    client.execute("UPDATE hudson_state SET state=jsonb_set(state,ARRAY['experiments',$2,'protocol'],'2'::jsonb) WHERE namespace=$1", &[&namespace, &experiment_key]).unwrap();
    assert!(matches!(
        connect().inspect_experiment(&actor(), id),
        Err(hudson_core::Error::Unsupported(_))
    ));
    client.execute("UPDATE hudson_state SET state=jsonb_set(state,ARRAY['experiments',$2,'protocol'],'1'::jsonb) WHERE namespace=$1", &[&namespace, &experiment_key]).unwrap();
    client.execute("UPDATE hudson_state SET state=jsonb_set(state,ARRAY['model_budgets'],'[]'::jsonb) WHERE namespace=$1", &[&namespace]).unwrap();
    assert!(Driver::resume(connect(), actor(), id).is_err());
    assert_eq!(fixture.requests.lock().unwrap().len(), 24);
}
