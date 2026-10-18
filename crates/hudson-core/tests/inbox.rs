use hudson_core::{
    adapters::{
        models::ModelExecutor,
        tools::{ExecutionError, ToolRegistry},
    },
    fixtures,
    inbox::MessageKind,
    models::*,
    runtime::Runtime,
    storage::Store,
};
use hudson_harness::{AgentLoop, Content, ModelRequest, ModelResponse};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

struct Recorder(Arc<Mutex<Vec<ModelRequest>>>);
impl ModelExecutor for Recorder {
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        self.0.lock().unwrap().push(request.clone());
        Ok(ModelResponse::Final {
            output: json!("verified answer"),
        })
    }
}
fn install(store: &Store) {
    let (mut agent, _) = fixtures::definitions();
    agent.tools.clear();
    agent.input_schema = None;
    agent.output_schema = None;
    store.publish_agent(agent).unwrap();
}
fn runtime(
    store: Store,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
) -> Runtime<AgentLoop, Recorder, ToolRegistry> {
    Runtime::new(store, AgentLoop, Recorder(requests), ToolRegistry::new())
}

fn exercise(connect: impl Fn() -> Store) {
    let actor = fixtures::actor();
    let store = connect();
    install(&store);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut host = runtime(store.clone(), requests.clone());
    let id = host
        .submit(&actor, fixtures::agent_ref(), json!("original"), None)
        .unwrap();
    let follow = store
        .enqueue_message(
            &actor,
            id,
            MessageKind::FollowUp,
            json!("follow up"),
            "follow",
        )
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
        .enqueue_message(&actor, id, MessageKind::Steer, json!("different"), "steer")
        .is_err());
    assert!(store
        .enqueue_message(
            &Actor {
                id: "other".into(),
                ..actor.clone()
            },
            id,
            MessageKind::Steer,
            json!("no"),
            "bad"
        )
        .is_err());
    host.tick(&actor, id).unwrap(); // Persist the first request and steering consumption.
    drop(host);
    drop(store);
    let store = connect();
    assert_eq!(
        store
            .enqueue_message(
                &actor,
                id,
                MessageKind::Steer,
                json!("new direction"),
                "steer"
            )
            .unwrap(),
        steer
    );
    assert_eq!(store.queued_messages(&actor, id).unwrap().len(), 1);
    let mut host = runtime(store.clone(), requests.clone());
    for _ in 0..30 {
        if host.tick(&actor, id).unwrap().status.terminal() {
            break;
        }
    }
    assert_eq!(
        store.inspect(&actor, id).unwrap().status,
        RunStatus::Completed
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let inputs = |request: &ModelRequest| {
        request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| {
                if let Content::Json { value } = content {
                    Some(value.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<Value>>()
    };
    assert_eq!(
        inputs(&requests[0]),
        vec![json!("original"), json!("new direction")]
    );
    assert_eq!(
        inputs(&requests[1]),
        vec![
            json!("original"),
            json!("new direction"),
            json!("verified answer"),
            json!("follow up")
        ]
    );
    assert_eq!(
        store
            .enqueue_message(
                &actor,
                id,
                MessageKind::FollowUp,
                json!("follow up"),
                "follow"
            )
            .unwrap(),
        follow
    );
    assert!(store
        .enqueue_message(&actor, id, MessageKind::FollowUp, json!("too late"), "late")
        .is_err());
    let events = store.events(&actor, id, 0).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::InputConsumed)
            .count(),
        2
    );
}

#[test]
fn steering_precedes_follow_up_and_receipts_survive_reopen() {
    let store = Store::default();
    exercise(|| store.clone());
}

#[test]
#[ignore = "requires HUDSON_TEST_DATABASE in local PostgreSQL"]
fn postgres_queue_consumption_and_receipts_survive_reconnect() {
    let database = std::env::var("HUDSON_TEST_DATABASE").unwrap();
    let namespace = format!("inbox-{}", uuid::Uuid::new_v4());
    exercise(|| Store::postgres_local("/tmp", &database, &namespace).unwrap());
}

#[test]
#[ignore = "requires HUDSON_TEST_DATABASE in local PostgreSQL"]
fn postgres_queue_state_promotes_its_format_and_rejects_unknown_versions() {
    let database = std::env::var("HUDSON_TEST_DATABASE").unwrap();
    let namespace = format!("inbox-format-{}", uuid::Uuid::new_v4());
    let mut client = postgres::Config::new()
        .host("/tmp")
        .dbname(&database)
        .connect_timeout(std::time::Duration::from_secs(10))
        .connect(postgres::NoTls)
        .unwrap();
    let store = Store::postgres_local("/tmp", &database, &namespace).unwrap();
    install(&store);
    let actor = fixtures::actor();
    let host = runtime(store.clone(), Default::default());
    let id = host
        .submit(&actor, fixtures::agent_ref(), json!("task"), None)
        .unwrap();
    let version = |client: &mut postgres::Client| -> i32 {
        client
            .query_one(
                "SELECT schema_version FROM hudson_state WHERE namespace=$1",
                &[&namespace],
            )
            .unwrap()
            .get(0)
    };
    assert_eq!(version(&mut client), 1);
    let receipt = store
        .enqueue_message(
            &actor,
            id,
            MessageKind::Steer,
            json!("direction"),
            "message",
        )
        .unwrap();
    assert_eq!(version(&mut client), 2);
    store.clear_queued_messages(&actor, id).unwrap();
    let reopened = Store::postgres_local("/tmp", &database, &namespace).unwrap();
    assert_eq!(
        reopened
            .enqueue_message(
                &actor,
                id,
                MessageKind::Steer,
                json!("direction"),
                "message"
            )
            .unwrap(),
        receipt
    );
    assert_eq!(version(&mut client), 2);
    client
        .execute(
            "UPDATE hudson_state SET schema_version=4 WHERE namespace=$1",
            &[&namespace],
        )
        .unwrap();
    assert!(matches!(
        store.inspect(&actor, id),
        Err(hudson_core::Error::Unsupported(_))
    ));
    assert!(matches!(
        Store::postgres_local("/tmp", &database, &namespace),
        Err(hudson_core::Error::Unsupported(_))
    ));
}

#[test]
fn queue_bounds_withdrawal_and_cancellation_do_not_forget_receipts() {
    let store = Store::default();
    install(&store);
    let actor = fixtures::actor();
    let host = runtime(store.clone(), Default::default());
    let id = host
        .submit(&actor, fixtures::agent_ref(), json!("task"), None)
        .unwrap();
    let first = store
        .enqueue_message(&actor, id, MessageKind::Steer, json!("direction"), "first")
        .unwrap();
    for index in 1..16 {
        store
            .enqueue_message(
                &actor,
                id,
                MessageKind::Steer,
                json!(index),
                &index.to_string(),
            )
            .unwrap();
    }
    assert!(store
        .enqueue_message(&actor, id, MessageKind::Steer, json!(17), "overflow")
        .is_err());
    assert_eq!(store.clear_queued_messages(&actor, id).unwrap(), 16);
    assert_eq!(
        store
            .enqueue_message(&actor, id, MessageKind::Steer, json!("direction"), "first")
            .unwrap(),
        first
    );
    assert!(store.queued_messages(&actor, id).unwrap().is_empty());
    assert!(store
        .enqueue_message(
            &actor,
            id,
            MessageKind::Steer,
            json!("x".repeat(65_536)),
            "too-large"
        )
        .is_err());
    store
        .enqueue_message(
            &actor,
            id,
            MessageKind::FollowUp,
            json!("later"),
            "cancelled",
        )
        .unwrap();
    host.cancel(&actor, id).unwrap();
    assert!(store.queued_messages(&actor, id).unwrap().is_empty());
    assert!(store
        .enqueue_message(&actor, id, MessageKind::Steer, json!("after cancel"), "no")
        .is_err());
}

#[test]
fn queued_input_cannot_bypass_approval_or_uncertain_outcomes() {
    for uncertain in [false, true] {
        let store = fixtures::store().unwrap();
        let actor = fixtures::actor();
        let tools = fixtures::FixtureTools {
            uncertain,
            ..Default::default()
        };
        let executed = tools.executed.clone();
        let mut host = Runtime::new(store.clone(), AgentLoop, fixtures::OrderModel, tools);
        let id = host
            .submit(
                &actor,
                fixtures::agent_ref(),
                json!({"order_id":"123","action":if uncertain {"lookup"} else {"refund"}}),
                None,
            )
            .unwrap();
        for _ in 0..12 {
            if host.tick(&actor, id).unwrap().status == RunStatus::Waiting {
                break;
            }
        }
        let before = store.inspect(&actor, id).unwrap();
        assert!(matches!(
            before.wait,
            Some(WaitReason::Approval { .. } | WaitReason::Reconciliation { .. })
        ));
        store
            .enqueue_message(
                &actor,
                id,
                MessageKind::Steer,
                json!({"approved":true,"answer":"not a reply"}),
                "steer",
            )
            .unwrap();
        for _ in 0..3 {
            host.tick(&actor, id).unwrap();
        }
        let after = store.inspect(&actor, id).unwrap();
        assert_eq!(after.wait, before.wait);
        assert_eq!(after.usage.model_calls, before.usage.model_calls);
        assert_eq!(store.queued_messages(&actor, id).unwrap().len(), 1);
        assert_eq!(executed.lock().unwrap().len(), usize::from(uncertain));
    }
}

#[test]
fn a_follow_up_enqueued_during_finalization_is_neither_lost_nor_duplicated() {
    use hudson_harness::{Backend, Checkpoint, Config, HarnessError, Input, Transition};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    struct PausedBoundary {
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        once: AtomicBool,
    }
    impl Backend for PausedBoundary {
        fn name(&self) -> &str {
            "hudson"
        }
        fn version(&self) -> u32 {
            1
        }
        fn advance(
            &self,
            config: &Config,
            checkpoint: &Checkpoint,
            input: Input,
        ) -> Result<Transition, HarnessError> {
            if matches!(input, Input::Verification { passed: true, .. })
                && self.once.swap(false, Ordering::SeqCst)
            {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            AgentLoop.advance(config, checkpoint, input)
        }
    }
    let store = Store::default();
    install(&store);
    let actor = fixtures::actor();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (entered, entered_rx) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::sync_channel(1);
    let mut host = Runtime::new(
        store.clone(),
        PausedBoundary {
            entered,
            release: Mutex::new(release_rx),
            once: AtomicBool::new(true),
        },
        Recorder(requests.clone()),
        ToolRegistry::new(),
    );
    let id = host
        .submit(&actor, fixtures::agent_ref(), json!("original"), None)
        .unwrap();
    for _ in 0..12 {
        if host
            .tick(&actor, id)
            .unwrap()
            .assessment
            .is_some_and(|assessment| assessment.passed)
        {
            break;
        }
    }
    assert_eq!(requests.lock().unwrap().len(), 1);
    let worker_actor = actor.clone();
    let worker = std::thread::spawn(move || host.tick(&worker_actor, id));
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    store
        .enqueue_message(
            &actor,
            id,
            MessageKind::FollowUp,
            json!("arrived during finalization"),
            "race",
        )
        .unwrap();
    release.send(()).unwrap();
    assert!(!worker.join().unwrap().unwrap().status.terminal());
    assert_eq!(store.queued_messages(&actor, id).unwrap().len(), 1);
    let mut host = runtime(store.clone(), requests.clone());
    for _ in 0..16 {
        if host.tick(&actor, id).unwrap().status.terminal() {
            break;
        }
    }
    assert_eq!(
        store.inspect(&actor, id).unwrap().status,
        RunStatus::Completed
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(store.queued_messages(&actor, id).unwrap().is_empty());
}

#[test]
fn withdrawal_fences_a_preparation_failure_without_suppressing_current_failures() {
    use hudson_harness::{Backend, Checkpoint, Config, HarnessError, Input, Transition};
    use std::sync::mpsc;

    struct PausedQueuedInput {
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl Backend for PausedQueuedInput {
        fn name(&self) -> &str {
            "hudson"
        }
        fn version(&self) -> u32 {
            1
        }
        fn advance(
            &self,
            config: &Config,
            checkpoint: &Checkpoint,
            input: Input,
        ) -> Result<Transition, HarnessError> {
            if matches!(input, Input::Queued { .. }) {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            AgentLoop.advance(config, checkpoint, input)
        }
    }

    for withdraw in [false, true] {
        let store = Store::default();
        let (mut agent, _) = fixtures::definitions();
        agent.tools.clear();
        agent.input_schema = None;
        agent.output_schema = None;
        agent.limits.max_context_bytes = 4096;
        store.publish_agent(agent).unwrap();
        let actor = fixtures::actor();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (entered, entered_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let mut host = Runtime::new(
            store.clone(),
            PausedQueuedInput {
                entered,
                release: Mutex::new(release_rx),
            },
            Recorder(requests.clone()),
            ToolRegistry::new(),
        );
        let id = host
            .submit(&actor, fixtures::agent_ref(), json!("original"), None)
            .unwrap();
        store
            .enqueue_message(
                &actor,
                id,
                MessageKind::Steer,
                json!("x".repeat(8192)),
                "oversized-context",
            )
            .unwrap();
        let worker_actor = actor.clone();
        let worker = std::thread::spawn(move || host.tick(&worker_actor, id));
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        if withdraw {
            assert_eq!(store.clear_queued_messages(&actor, id).unwrap(), 1);
        }
        release.send(()).unwrap();
        let view = worker.join().unwrap().unwrap();
        assert!(requests.lock().unwrap().is_empty());
        if !withdraw {
            assert_eq!(view.status, RunStatus::Failed);
            continue;
        }
        assert!(!view.status.terminal());
        let mut host = runtime(store.clone(), requests.clone());
        for _ in 0..12 {
            if host.tick(&actor, id).unwrap().status.terminal() {
                break;
            }
        }
        assert_eq!(
            store.inspect(&actor, id).unwrap().status,
            RunStatus::Completed
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(matches!(requests[0].messages[0].content.as_slice(),
            [Content::Json { value }] if value == &json!("original")));
        assert!(!store
            .events(&actor, id, 0)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event_type, EventType::InputConsumed)
                || event.event_type == EventType::RunFinished
                    && event.payload["status"] == "failed"));
    }
}

#[test]
fn consumed_keys_remain_bounded_and_aggregate_payload_overflow_rolls_back() {
    let store = Store::default();
    install(&store);
    let actor = fixtures::actor();
    let host = runtime(store.clone(), Default::default());
    let id = host
        .submit(&actor, fixtures::agent_ref(), json!("task"), None)
        .unwrap();
    for index in 0..128 {
        store
            .enqueue_message(
                &actor,
                id,
                MessageKind::Steer,
                json!(index),
                &format!("key-{index}"),
            )
            .unwrap();
        store.clear_queued_messages(&actor, id).unwrap();
    }
    assert!(store
        .enqueue_message(&actor, id, MessageKind::Steer, json!(129), "overflow")
        .is_err());
    store
        .enqueue_message(&actor, id, MessageKind::Steer, json!(0), "key-0")
        .unwrap();
    assert!(store.queued_messages(&actor, id).unwrap().is_empty());
    let other = host
        .submit(&actor, fixtures::agent_ref(), json!("another task"), None)
        .unwrap();
    store
        .enqueue_message(
            &actor,
            other,
            MessageKind::Steer,
            json!("x".repeat(40_000)),
            "large-1",
        )
        .unwrap();
    assert!(store
        .enqueue_message(
            &actor,
            other,
            MessageKind::Steer,
            json!("y".repeat(40_000)),
            "large-2"
        )
        .is_err());
    assert_eq!(store.queued_messages(&actor, other).unwrap().len(), 1);
    store
        .enqueue_message(&actor, other, MessageKind::Steer, json!("small"), "large-2")
        .unwrap();
}
