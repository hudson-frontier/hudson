#![cfg(feature = "fixtures")]
use hudson_core::{
    adapters::{models::ModelExecutor, tools::ExecutionError},
    fixtures::{self, FixtureTools},
    models::*,
    runtime::Runtime,
    security::Policy,
    spending::{Quota, Rates, Scope, SpendPolicy, SpendStatus},
    storage::Store,
};
use hudson_harness::{AgentLoop, ModelRequest, ModelResponse, ToolCall};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

struct Model {
    responses: VecDeque<ModelResponse>,
    usage: Option<TokenUsage>,
    reported: Option<TokenUsage>,
    fail: bool,
    calls: Arc<AtomicUsize>,
}
impl ModelExecutor for Model {
    fn call(&mut self, _: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.reported = self.usage.clone();
        if self.fail {
            Err(ExecutionError::Unknown("lost fixture response".into()))
        } else {
            Ok(self
                .responses
                .pop_front()
                .unwrap_or(ModelResponse::Final { output: json!(42) }))
        }
    }
    fn take_usage(&mut self) -> Option<TokenUsage> {
        self.reported.take()
    }
}
fn model(usage: Option<u64>, fail: bool, responses: Vec<ModelResponse>) -> Model {
    Model {
        responses: responses.into(),
        usage: usage.map(|input_tokens| TokenUsage {
            input_tokens,
            output_tokens: 0,
        }),
        reported: None,
        fail,
        calls: Default::default(),
    }
}
fn policy(limit: u64, soft: Option<u64>) -> SpendPolicy {
    SpendPolicy {
        allocation_nano_usd: 100,
        rates: Rates {
            input_nano_usd_per_million_tokens: 1_000_000,
            output_nano_usd_per_million_tokens: 0,
        },
        quotas: vec![Quota {
            id: "workspace".into(),
            scope: Scope::Workspace,
            limit_nano_usd: limit,
            soft_nano_usd: soft,
            approvers: ["developer".into()].into(),
        }],
    }
}
fn install(store: &Store, policy: &SpendPolicy) {
    let (mut agent, tools) = fixtures::definitions();
    agent.input_schema = None;
    agent.output_schema = None;
    for tool in tools {
        store
            .set_policy(
                "demo",
                &tool.policy_ref,
                Policy {
                    actors: ["developer".into(), "other".into()].into(),
                    approvers: ["developer".into()].into(),
                    require_approval: tool.effect == Effect::Write,
                },
            )
            .unwrap();
        store.publish_tool(tool).unwrap();
    }
    store.publish_agent(agent).unwrap();
    store
        .bind_spend_policy("demo", &fixtures::agent_ref(), Some(policy))
        .unwrap();
}
fn runtime(store: Store, model: Model) -> Runtime<AgentLoop, Model, FixtureTools> {
    Runtime::new(store, AgentLoop, model, FixtureTools::default())
}
fn submit(runtime: &Runtime<AgentLoop, Model, FixtureTools>, actor: &Actor) -> uuid::Uuid {
    runtime
        .submit(actor, fixtures::agent_ref(), json!("fixture task"), None)
        .unwrap()
}

#[test]
fn soft_threshold_requests_model_approval_before_io_and_prices_reports_once() {
    let store = Store::default();
    install(&store, &policy(1000, Some(100)));
    let model = model(
        Some(10),
        false,
        vec![ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                call_id: "read-1".into(),
                name: "lookup_order".into(),
                arguments: json!({"order_id":"123"}),
                provider_metadata: serde_json::Value::Null,
            }],
        }],
    );
    let calls = model.calls.clone();
    let mut runtime = runtime(store.clone(), model);
    let actor = fixtures::actor();
    let id = submit(&runtime, &actor);
    let wait = fixtures::drive(&mut runtime, &actor, id).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let Some(WaitReason::Approval { operation_id }) = wait.wait else {
        panic!("expected priced model approval");
    };
    assert_eq!(
        store.inspect_operation(&actor, operation_id).unwrap().kind,
        "model"
    );
    let before = store.run_spend(&actor, id).unwrap();
    assert_eq!(before.entries.len(), 2);
    assert_eq!(
        before
            .entries
            .iter()
            .filter(|e| e.status == SpendStatus::Reserved)
            .count(),
        1
    );
    assert!(runtime
        .approve(
            &Actor {
                id: "intruder".into(),
                ..actor.clone()
            },
            operation_id,
            true,
            now(),
            now() + 60000
        )
        .is_err());
    runtime
        .approve(&actor, operation_id, true, now(), now() + 60000)
        .unwrap();
    assert_eq!(
        fixtures::drive(&mut runtime, &actor, id).unwrap().status,
        RunStatus::Completed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let entries = store.run_spend(&actor, id).unwrap().entries;
    assert!(entries.iter().all(
        |e| e.status == SpendStatus::Priced && e.priced_usage_nano_usd.as_deref() == Some("10")
    ));
    runtime.tick(&actor, id).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(store.run_spend(&actor, id).unwrap().entries.len(), 2);
    assert!(store
        .run_spend(
            &Actor {
                id: "foreign".into(),
                ..actor
            },
            id
        )
        .is_err());
}

fn unknown_cycle(store: Store, reopen: impl FnOnce() -> Store) {
    install(&store, &policy(190, None));
    let actor = fixtures::actor();
    let model = model(None, true, vec![]);
    let calls = model.calls.clone();
    let mut first = runtime(store.clone(), model);
    let id = submit(&first, &actor);
    assert!(matches!(
        fixtures::drive(&mut first, &actor, id).unwrap().wait,
        Some(WaitReason::Reconciliation { .. })
    ));
    let entry = store.run_spend(&actor, id).unwrap().entries.remove(0);
    assert_eq!(entry.status, SpendStatus::Unknown);
    assert!(entry.priced_usage_nano_usd.is_none());
    drop(first);
    let store = reopen();
    let mut next = runtime(
        store.clone(),
        Model {
            responses: Default::default(),
            usage: None,
            reported: None,
            fail: false,
            calls: calls.clone(),
        },
    );
    let denied = submit(&next, &actor);
    assert_eq!(
        fixtures::drive(&mut next, &actor, denied).unwrap().status,
        RunStatus::Failed
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "unknown allocations cannot be reused"
    );
    let attempt = entry.attempt_id.unwrap();
    store
        .reconcile_spend_usage(
            &actor,
            entry.operation_id,
            attempt,
            TokenUsage {
                input_tokens: 10,
                output_tokens: 0,
            },
            "verified fixture provider report",
        )
        .unwrap();
    store
        .reconcile_spend_usage(
            &actor,
            entry.operation_id,
            attempt,
            TokenUsage {
                input_tokens: 10,
                output_tokens: 0,
            },
            "verified fixture provider report",
        )
        .unwrap();
    assert_eq!(
        store
            .operation_spend(&actor, entry.operation_id)
            .unwrap()
            .unwrap()
            .priced_usage_nano_usd
            .as_deref(),
        Some("10")
    );
    assert!(store
        .reconcile_spend_usage(
            &actor,
            entry.operation_id,
            attempt,
            TokenUsage {
                input_tokens: 11,
                output_tokens: 0
            },
            "changed report"
        )
        .is_err());
    let admitted = submit(&next, &actor);
    assert_eq!(
        fixtures::drive(&mut next, &actor, admitted).unwrap().status,
        RunStatus::Completed
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "verified pricing releases only the unused allocation"
    );
}
#[test]
fn unknown_usage_retains_allocation_and_exact_reconciliation_is_idempotent() {
    let store = Store::default();
    unknown_cycle(store.clone(), || store);
}
#[test]
#[ignore = "requires dedicated PostgreSQL database on /tmp"]
fn postgres_reopen_preserves_unknown_allocations_and_reconciliation() {
    let database = std::env::var("HUDSON_TEST_DATABASE").expect("dedicated test database");
    let namespace = format!("spend-{}", uuid::Uuid::new_v4());
    unknown_cycle(
        Store::postgres_local("/tmp", &database, &namespace).unwrap(),
        || Store::postgres_local("/tmp", &database, &namespace).unwrap(),
    );
    let mut client = postgres::Config::new()
        .host("/tmp")
        .dbname(&database)
        .connect(postgres::NoTls)
        .unwrap();
    let version: i32 = client
        .query_one(
            "SELECT schema_version FROM hudson_state WHERE namespace=$1",
            &[&namespace],
        )
        .unwrap()
        .get(0);
    assert_eq!(
        version, 4,
        "legacy writers must reject spend-enabled namespaces"
    );
}

#[cfg(unix)]
#[test]
fn local_snapshot_reopens_spend_state_and_keeps_its_version_after_writes() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("state.json");
    let actor = fixtures::actor();
    let run = {
        let store = Store::local_file(&path).unwrap();
        install(&store, &policy(100, None));
        let mut runtime = runtime(store, model(None, true, vec![]));
        let id = submit(&runtime, &actor);
        fixtures::drive(&mut runtime, &actor, id).unwrap();
        id
    };
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["schema_version"], 2);
    let store = Store::local_file(&path).unwrap();
    assert_eq!(
        store.run_spend(&actor, run).unwrap().entries[0].status,
        SpendStatus::Unknown
    );
    store
        .set_policy("demo", "unrelated", Policy::default())
        .unwrap();
    drop(store);
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["schema_version"], 2);
}

#[cfg(unix)]
#[test]
fn an_empty_upgraded_local_snapshot_never_downgrades_its_envelope() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("empty.json");
    {
        let store = Store::local_file(&path).unwrap();
        store
            .set_policy("demo", "initial", Policy::default())
            .unwrap();
    }
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["schema_version"] = json!(2);
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    {
        let store = Store::local_file(&path).unwrap();
        store.set_policy("demo", "next", Policy::default()).unwrap();
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["schema_version"], 2);
}
#[test]
fn overrun_freezes_allocation_scopes_without_dispatching_returned_tools() {
    let store = Store::default();
    install(&store, &policy(1000, None));
    let actor = fixtures::actor();
    let model = model(
        Some(101),
        false,
        vec![ModelResponse::ToolCalls {
            calls: vec![ToolCall {
                call_id: "never-run".into(),
                name: "lookup_order".into(),
                arguments: json!({"order_id":"123"}),
                provider_metadata: serde_json::Value::Null,
            }],
        }],
    );
    let calls = model.calls.clone();
    let mut runtime = runtime(store.clone(), model);
    let id = submit(&runtime, &actor);
    assert_eq!(
        fixtures::drive(&mut runtime, &actor, id).unwrap().status,
        RunStatus::Failed
    );
    assert_eq!(store.inspect(&actor, id).unwrap().usage.tool_calls, 0);
    assert!(store.run_spend(&actor, id).unwrap().entries[0].overrun);
    let next = submit(&runtime, &actor);
    assert_eq!(
        fixtures::drive(&mut runtime, &actor, next).unwrap().status,
        RunStatus::Failed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[test]
fn cancellation_releases_only_unstarted_model_allocations() {
    let store = Store::default();
    install(&store, &policy(100, Some(1)));
    let actor = fixtures::actor();
    let model = model(Some(0), false, vec![]);
    let calls = model.calls.clone();
    let mut runtime = runtime(store.clone(), model);
    let id = submit(&runtime, &actor);
    assert!(matches!(
        fixtures::drive(&mut runtime, &actor, id).unwrap().wait,
        Some(WaitReason::Approval { .. })
    ));
    runtime.cancel(&actor, id).unwrap();
    assert_eq!(
        store.run_spend(&actor, id).unwrap().entries[0].status,
        SpendStatus::Released
    );
    let next = submit(&runtime, &actor);
    assert!(matches!(
        fixtures::drive(&mut runtime, &actor, next).unwrap().wait,
        Some(WaitReason::Approval { .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn denial_in_one_scope_does_not_partially_reserve_another_scope() {
    let store = Store::default();
    let mut quotas = policy(200, None);
    quotas.quotas.push(Quota {
        id: "daily".into(),
        scope: Scope::ActorDay,
        limit_nano_usd: 100,
        soft_nano_usd: None,
        approvers: Default::default(),
    });
    install(&store, &quotas);
    let actor = fixtures::actor();
    let mut first = runtime(store.clone(), model(None, true, vec![]));
    let a = submit(&first, &actor);
    assert!(matches!(
        fixtures::drive(&mut first, &actor, a).unwrap().wait,
        Some(WaitReason::Reconciliation { .. })
    ));
    let deny_model = model(None, false, vec![]);
    let calls = deny_model.calls.clone();
    let mut second = runtime(store.clone(), deny_model);
    let b = submit(&second, &actor);
    assert_eq!(
        fixtures::drive(&mut second, &actor, b).unwrap().status,
        RunStatus::Failed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let other = Actor {
        id: "other".into(),
        ..actor
    };
    let mut third = runtime(store, model(None, false, vec![]));
    let c = submit(&third, &other);
    assert_eq!(
        fixtures::drive(&mut third, &other, c).unwrap().status,
        RunStatus::Completed
    );
}

#[test]
fn policy_binding_is_immutable_and_large_currency_values_roundtrip_exactly() {
    let store = Store::default();
    let mut configuration = policy(u64::MAX, None);
    configuration.allocation_nano_usd = (1_u64 << 53) + 1;
    install(&store, &configuration);
    assert!(store
        .bind_spend_policy("demo", &fixtures::agent_ref(), None)
        .is_err());
    let mut changed = configuration.clone();
    changed.rates.input_nano_usd_per_million_tokens += 1;
    assert!(store
        .bind_spend_policy("demo", &fixtures::agent_ref(), Some(&changed))
        .is_err());
    let actor = fixtures::actor();
    let mut runtime = runtime(store.clone(), model(Some(0), false, vec![]));
    let id = submit(&runtime, &actor);
    assert_eq!(
        fixtures::drive(&mut runtime, &actor, id).unwrap().status,
        RunStatus::Completed
    );
    let entry = store.run_spend(&actor, id).unwrap().entries.remove(0);
    assert_eq!(entry.allocation_nano_usd, "9007199254740993");
    let json = serde_json::to_value(&entry).unwrap();
    assert_eq!(json["allocation_nano_usd"], "9007199254740993");
    assert_eq!(entry.priced_usage_nano_usd.as_deref(), Some("0"));
}
