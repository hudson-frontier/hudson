use super::*;
use crate::fixtures;

#[test]
fn fixed_point_pricing_rounds_up_and_fails_closed_on_overflow() {
    let rates = Rates {
        input_nano_usd_per_million_tokens: 1,
        output_nano_usd_per_million_tokens: 0,
    };
    assert_eq!(
        rates.price(&TokenUsage {
            input_tokens: 1,
            output_tokens: 0
        }),
        Some(1)
    );
    assert_eq!(
        rates.price(&TokenUsage {
            input_tokens: 1_000_001,
            output_tokens: 0
        }),
        Some(2)
    );
    assert_eq!(
        rates.price(&TokenUsage {
            input_tokens: 0,
            output_tokens: 0
        }),
        Some(0)
    );
    let rates = Rates {
        input_nano_usd_per_million_tokens: u64::MAX,
        output_nano_usd_per_million_tokens: u64::MAX,
    };
    assert_eq!(
        rates.price(&TokenUsage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX
        }),
        None
    );
}
#[test]
fn quota_keys_keep_workspace_agent_run_and_utc_admission_days_separate() {
    let runtime = fixtures::runtime().unwrap();
    let actor = fixtures::actor();
    let id = runtime
        .submit(
            &actor,
            fixtures::agent_ref(),
            serde_json::json!({"order_id":"123","action":"lookup"}),
            None,
        )
        .unwrap();
    let run = runtime.store.read(|d| Ok(d.runs[&id].clone())).unwrap();
    let quota = |scope| Quota {
        id: "pool".into(),
        scope,
        limit_nano_usd: 100,
        soft_nano_usd: None,
        approvers: Default::default(),
    };
    assert_ne!(
        account_key(&run, &quota(Scope::ActorDay), 0).unwrap(),
        account_key(&run, &quota(Scope::ActorDay), 86_400_000).unwrap()
    );
    let mut other = run.clone();
    other.actor_id = "other".into();
    other.meta.id = Uuid::new_v4();
    assert_eq!(
        account_key(&run, &quota(Scope::Workspace), 0).unwrap(),
        account_key(&other, &quota(Scope::Workspace), 86_400_000).unwrap()
    );
    assert_eq!(
        account_key(&run, &quota(Scope::AgentVersion), 0).unwrap(),
        account_key(&other, &quota(Scope::AgentVersion), 0).unwrap()
    );
    assert_ne!(
        account_key(&run, &quota(Scope::Run), 0).unwrap(),
        account_key(&other, &quota(Scope::Run), 0).unwrap()
    );
    other.agent_ref.version += 1;
    assert_ne!(
        account_key(&run, &quota(Scope::AgentVersion), 0).unwrap(),
        account_key(&other, &quota(Scope::AgentVersion), 0).unwrap()
    );
}
