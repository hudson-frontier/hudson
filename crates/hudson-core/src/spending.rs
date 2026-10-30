//! Fixed-point allocation admission. Priced usage uses operator-pinned rates;
//! reservations and missing reports are never presented as provider invoices.
use crate::{
    definitions::digest,
    models::*,
    security::Admission,
    storage::{memory::Data, Store},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const NANO_USD_PER_USD: u64 = 1_000_000_000;
#[cfg(all(test, feature = "fixtures"))]
mod tests;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Run,
    AgentVersion,
    Workspace,
    ActorDay,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Quota {
    pub id: String,
    pub scope: Scope,
    pub limit_nano_usd: u64,
    #[serde(default)]
    pub soft_nano_usd: Option<u64>,
    #[serde(default)]
    pub approvers: BTreeSet<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rates {
    pub input_nano_usd_per_million_tokens: u64,
    pub output_nano_usd_per_million_tokens: u64,
}
impl Rates {
    pub fn price(self, usage: &TokenUsage) -> Option<u64> {
        let input = u128::from(usage.input_tokens)
            .checked_mul(u128::from(self.input_nano_usd_per_million_tokens))?;
        let output = u128::from(usage.output_tokens)
            .checked_mul(u128::from(self.output_nano_usd_per_million_tokens))?;
        let total = input.checked_add(output)?;
        u64::try_from(total / 1_000_000 + u128::from(total % 1_000_000 != 0)).ok()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpendPolicy {
    /// Allocation held before each model attempt, not an inferred invoice cap.
    pub allocation_nano_usd: u64,
    pub rates: Rates,
    pub quotas: Vec<Quota>,
}
impl SpendPolicy {
    pub fn validate(&self) -> Result<()> {
        let mut ids = BTreeSet::new();
        if self.allocation_nano_usd == 0 || self.quotas.is_empty() || self.quotas.len() > 4 {
            return Err(Error::Invalid(
                "spend policy requires a positive allocation and one to four quotas".into(),
            ));
        }
        for q in &self.quotas {
            if q.id.is_empty()
                || q.id.len() > 64
                || !q
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                || !ids.insert(&q.id)
                || q.limit_nano_usd < self.allocation_nano_usd
                || q.soft_nano_usd
                    .is_some_and(|s| s == 0 || s > q.limit_nano_usd || q.approvers.is_empty())
                || q.approvers.len() > 64
                || q.approvers.iter().any(|a| a.is_empty() || a.len() > 256)
            {
                return Err(Error::Invalid(
                    "invalid spend quota identity, threshold or approvers".into(),
                ));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Account {
    policy_digest: String,
    held: u64,
    priced: u64,
    frozen: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpendStatus {
    Reserved,
    InFlight,
    Priced,
    Unknown,
    Released,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpendEntry {
    pub quoted_at: u64,
    pub operation_id: Uuid,
    pub request_digest: String,
    pub policy_digest: String,
    pub allocation_nano_usd: u64,
    pub priced_usage_nano_usd: Option<u64>,
    pub status: SpendStatus,
    pub attempt_id: Option<Uuid>,
    pub overrun: bool,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub required_approvers: BTreeSet<String>,
    #[serde(default)]
    pub(crate) accounts: Vec<String>,
    pub(crate) rates: Rates,
    #[serde(default)]
    pub(crate) reconciliation_digest: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RunSpend {
    pub enabled: bool,
    pub currency: &'static str,
    pub units_per_usd: u64,
    pub entries: Vec<SpendView>,
}
#[derive(Clone, Debug, Serialize)]
pub struct SpendView {
    pub quoted_at: u64,
    pub operation_id: Uuid,
    pub allocation_nano_usd: String,
    pub priced_usage_nano_usd: Option<String>,
    pub status: SpendStatus,
    pub overrun: bool,
    pub attempt_id: Option<Uuid>,
}

impl From<&SpendEntry> for SpendView {
    fn from(e: &SpendEntry) -> Self {
        Self {
            quoted_at: e.quoted_at,
            operation_id: e.operation_id,
            allocation_nano_usd: e.allocation_nano_usd.to_string(),
            priced_usage_nano_usd: e.priced_usage_nano_usd.map(|n| n.to_string()),
            status: e.status,
            overrun: e.overrun,
            attempt_id: e.attempt_id,
        }
    }
}

fn bind_policy(
    d: &mut Data,
    workspace: &str,
    agent: &VersionRef,
    policy: Option<&SpendPolicy>,
) -> Result<()> {
    let key = (workspace.to_owned(), agent.clone());
    if !d.agents.contains_key(&key) {
        return Err(Error::NotFound);
    }
    if let Some(existing) = d.spend_policies.get(&key) {
        if Some(existing) != policy {
            return Err(Error::Conflict(
                "spend policy changed; publish a new agent version".into(),
            ));
        }
    } else if let Some(policy) = policy {
        if d.runs
            .values()
            .any(|r| r.meta.workspace_id == workspace && &r.agent_ref == agent)
        {
            return Err(Error::Conflict(
                "cannot enable spending after runs start; publish a new agent version".into(),
            ));
        }
        d.spend_policies.insert(key, policy.clone());
    }
    Ok(())
}
impl Store {
    /// Operator configuration only. A version cannot change or remove pricing.
    pub fn bind_spend_policy(
        &self,
        workspace: &str,
        agent: &VersionRef,
        policy: Option<&SpendPolicy>,
    ) -> Result<()> {
        if let Some(policy) = policy {
            policy.validate()?;
        }
        self.transact(|d| bind_policy(d, workspace, agent, policy))
    }
    pub(crate) fn bind_priced_transport(
        &self,
        workspace: &str,
        agent: &VersionRef,
        fingerprint: &str,
        policy: Option<&SpendPolicy>,
    ) -> Result<()> {
        if let Some(policy) = policy {
            policy.validate()?;
        }
        self.transact(|d| {
            let key = (workspace.to_owned(), agent.clone());
            if d.model_bindings
                .get(&key)
                .is_some_and(|old| old != fingerprint)
            {
                return Err(Error::Conflict(
                    "agent model transport changed; publish a new version".into(),
                ));
            }
            bind_policy(d, workspace, agent, policy)?;
            d.model_bindings
                .entry(key)
                .or_insert_with(|| fingerprint.to_owned());
            Ok(())
        })
    }
    pub fn run_spend(&self, actor: &Actor, run: Uuid) -> Result<RunSpend> {
        self.read(|d| {
            d.run(actor, run)?;
            let entries = d
                .spend_entries
                .get(&run)
                .into_iter()
                .flat_map(BTreeMap::values)
                .map(SpendView::from)
                .collect();
            Ok(RunSpend {
                enabled: d
                    .spend_policies
                    .contains_key(&(actor.workspace_id.clone(), d.runs[&run].agent_ref.clone())),
                currency: "USD",
                units_per_usd: NANO_USD_PER_USD,
                entries,
            })
        })
    }
    pub fn operation_spend(&self, actor: &Actor, id: Uuid) -> Result<Option<SpendView>> {
        self.read(|d| {
            let operation = d.operations.get(&id).ok_or(Error::NotFound)?;
            d.run(actor, operation.run_id)?;
            Ok(d.spend_entries
                .get(&operation.run_id)
                .and_then(|e| e.get(&id))
                .map(SpendView::from))
        })
    }
    /// Trusted operator reconciliation, deliberately absent from HTTP controls.
    /// The original executor must have stopped; record that fact before pricing
    /// an interrupted attempt. Missing usage cannot be replaced with zero.
    pub fn reconcile_spend_usage(
        &self,
        actor: &Actor,
        operation: Uuid,
        attempt: Uuid,
        usage: TokenUsage,
        evidence: &str,
    ) -> Result<()> {
        if evidence.trim().is_empty() || evidence.len() > 4096 {
            return Err(Error::Invalid(
                "verified spend evidence is required (at most 4 KiB)".into(),
            ));
        }
        let receipt = digest(&(&actor.id, operation, attempt, &usage, evidence))?;
        self.transact(|d| {
            let op = d.operations.get(&operation).ok_or(Error::NotFound)?;
            d.run(actor, op.run_id)?;
            if op.status == OperationStatus::Running
                || op.attempts.last().map(|a| a.id) != Some(attempt)
            {
                return Err(Error::Conflict(
                    "stop and fence the exact model attempt before spend reconciliation".into(),
                ));
            }
            if op
                .attempts
                .last()
                .and_then(|a| a.token_usage.as_ref())
                .is_some_and(|old| old != &usage)
            {
                return Err(Error::Conflict(
                    "verified usage conflicts with an existing attempt report".into(),
                ));
            }
            let run = op.run_id;
            let entry = d
                .spend_entries
                .get_mut(&run)
                .and_then(|e| e.get_mut(&operation))
                .ok_or(Error::NotFound)?;
            if entry.reconciliation_digest.as_ref() == Some(&receipt) {
                return Ok(());
            }
            if entry.status != SpendStatus::Unknown || entry.attempt_id != Some(attempt) {
                return Err(Error::Conflict(
                    "spend receipt changed or already priced".into(),
                ));
            }
            entry.status = SpendStatus::InFlight;
            settle(d, run, operation, attempt, Some(&usage))?;
            let operation_record = d.operations.get_mut(&operation).unwrap();
            let attempt_record = operation_record.attempts.last_mut().unwrap();
            if attempt_record.token_usage.is_none() {
                attempt_record.token_usage = Some(usage.clone());
                operation_record.revision += 1;
                let run_record = d.runs.get_mut(&run).unwrap();
                run_record.usage.reported_model_calls =
                    run_record.usage.reported_model_calls.saturating_add(1);
                run_record.usage.reported_input_tokens = run_record
                    .usage
                    .reported_input_tokens
                    .saturating_add(usage.input_tokens);
                run_record.usage.reported_output_tokens = run_record
                    .usage
                    .reported_output_tokens
                    .saturating_add(usage.output_tokens);
                run_record.revision += 1;
            }
            d.spend_entries
                .get_mut(&run)
                .unwrap()
                .get_mut(&operation)
                .unwrap()
                .reconciliation_digest = Some(receipt);
            d.event(
                run,
                Some(operation),
                EventType::OperationSettled,
                Some(actor.id.clone()),
                serde_json::json!({"spend_reconciled":true}),
            );
            Ok(())
        })
    }
}
fn account_key(run: &Run, quota: &Quota, at: u64) -> Result<String> {
    let owner = match quota.scope {
        Scope::Run => run.meta.id.to_string(),
        Scope::AgentVersion => digest(&run.agent_ref)?,
        Scope::Workspace => String::new(),
        Scope::ActorDay => digest(&(&run.actor_id, at / 86_400_000))?,
    };
    digest(&(&run.meta.workspace_id, &quota.id, quota.scope, owner))
}
pub(crate) fn admit(d: &mut Data, run: &Run, operation: &Operation, at: u64) -> Result<Admission> {
    let Some(policy) = d
        .spend_policies
        .get(&(run.meta.workspace_id.clone(), run.agent_ref.clone()))
    else {
        return Ok(Admission::Allow);
    };
    let policy_digest = digest(&policy)?;
    if let Some(entry) = d
        .spend_entries
        .get(&run.meta.id)
        .and_then(|entries| entries.get(&operation.meta.id))
    {
        if entry.request_digest != operation.request_digest
            || entry.policy_digest != policy_digest
            || entry.status != SpendStatus::Reserved
        {
            return Ok(Admission::Deny);
        }
        if entry
            .accounts
            .iter()
            .any(|key| d.spend_accounts.get(key).is_none_or(|a| a.frozen))
        {
            return Ok(Admission::Deny);
        }
        return Ok(if approved(operation, &entry.required_approvers, at) {
            Admission::Allow
        } else {
            Admission::Wait
        });
    }
    let mut required: BTreeSet<String> = BTreeSet::new();
    let mut accounts = Vec::new();
    // Validate every scope before reserving any of them.
    for quota in &policy.quotas {
        let key = account_key(run, quota, at)?;
        let quote_digest = digest(quota)?;
        let account = d.spend_accounts.get(&key);
        if account.is_some_and(|a| a.frozen || a.policy_digest != quote_digest) {
            return Ok(Admission::Deny);
        }
        let total = account
            .map_or(Some(0), |a| a.held.checked_add(a.priced))
            .and_then(|used| used.checked_add(policy.allocation_nano_usd));
        let Some(total) = total.filter(|total| *total <= quota.limit_nano_usd) else {
            return Ok(Admission::Deny);
        };
        if quota.soft_nano_usd.is_some_and(|soft| total > soft) {
            required = if required.is_empty() {
                quota.approvers.clone()
            } else {
                required.intersection(&quota.approvers).cloned().collect()
            };
            if required.is_empty() {
                return Ok(Admission::Deny);
            }
        }
        accounts.push((key, quote_digest));
    }
    for (key, quote_digest) in &accounts {
        let account = d
            .spend_accounts
            .entry(key.clone())
            .or_insert_with(|| Account {
                policy_digest: quote_digest.clone(),
                ..Account::default()
            });
        account.held = account
            .held
            .checked_add(policy.allocation_nano_usd)
            .ok_or_else(|| Error::Invalid("spend allocation overflow".into()))?;
    }
    let allowed = approved(operation, &required, at);
    d.spend_entries.entry(run.meta.id).or_default().insert(
        operation.meta.id,
        SpendEntry {
            quoted_at: at,
            operation_id: operation.meta.id,
            request_digest: operation.request_digest.clone(),
            policy_digest,
            allocation_nano_usd: policy.allocation_nano_usd,
            priced_usage_nano_usd: None,
            status: SpendStatus::Reserved,
            attempt_id: None,
            overrun: false,
            required_approvers: required,
            accounts: accounts.into_iter().map(|(key, _)| key).collect(),
            rates: policy.rates,
            reconciliation_digest: None,
        },
    );
    Ok(if allowed {
        Admission::Allow
    } else {
        Admission::Wait
    })
}
fn approved(operation: &Operation, required: &BTreeSet<String>, at: u64) -> bool {
    required.is_empty()
        || operation
            .approval
            .as_ref()
            .filter(|a| a.request_digest == operation.request_digest)
            .and_then(|a| a.decisions.last())
            .is_some_and(|a| {
                a.approved
                    && a.request_digest == operation.request_digest
                    && a.decided_at <= at
                    && at < a.expires_at
                    && required.contains(&a.actor_id)
            })
}
pub(crate) fn start(d: &mut Data, run: Uuid, operation: Uuid, attempt: Uuid) {
    if let Some(entry) = d
        .spend_entries
        .get_mut(&run)
        .and_then(|e| e.get_mut(&operation))
    {
        entry.status = SpendStatus::InFlight;
        entry.attempt_id = Some(attempt);
    }
}
pub(crate) fn settle(
    d: &mut Data,
    run: Uuid,
    operation: Uuid,
    attempt: Uuid,
    usage: Option<&TokenUsage>,
) -> Result<bool> {
    let Some(entry) = d
        .spend_entries
        .get(&run)
        .and_then(|e| e.get(&operation))
        .cloned()
    else {
        return Ok(false);
    };
    if entry.attempt_id != Some(attempt) || entry.status != SpendStatus::InFlight {
        return Err(Error::Conflict("stale spend receipt".into()));
    }
    let priced = usage.and_then(|u| entry.rates.price(u));
    let overflow = usage.is_some() && priced.is_none();
    let account_overflow = entry.accounts.iter().any(|key| {
        d.spend_accounts
            .get(key)
            .is_none_or(|a| priced.is_some_and(|amount| a.priced.checked_add(amount).is_none()))
    });
    let overrun = overflow
        || account_overflow
        || priced.is_some_and(|amount| amount > entry.allocation_nano_usd);
    for key in &entry.accounts {
        let account = d.spend_accounts.get_mut(key).ok_or(Error::NotFound)?;
        if let Some(priced) = priced.filter(|_| !account_overflow) {
            account.held = account
                .held
                .checked_sub(entry.allocation_nano_usd)
                .ok_or_else(|| Error::Conflict("spend account underflow".into()))?;
            account.priced = account
                .priced
                .checked_add(priced)
                .ok_or_else(|| Error::Conflict("spend account overflow".into()))?;
        }
        account.frozen |= overrun;
    }
    let stored = d
        .spend_entries
        .get_mut(&run)
        .unwrap()
        .get_mut(&operation)
        .unwrap();
    stored.priced_usage_nano_usd = priced;
    stored.status = if priced.is_some() && !account_overflow {
        SpendStatus::Priced
    } else {
        SpendStatus::Unknown
    };
    stored.overrun = overrun;
    Ok(overrun)
}
pub(crate) fn release(d: &mut Data, run: Uuid, operation: Uuid) -> Result<()> {
    let Some(entry) = d
        .spend_entries
        .get(&run)
        .and_then(|e| e.get(&operation))
        .cloned()
        .filter(|e| e.status == SpendStatus::Reserved && e.attempt_id.is_none())
    else {
        return Ok(());
    };
    for key in &entry.accounts {
        let account = d.spend_accounts.get_mut(key).ok_or(Error::NotFound)?;
        account.held = account
            .held
            .checked_sub(entry.allocation_nano_usd)
            .ok_or_else(|| Error::Conflict("spend account underflow".into()))?;
    }
    d.spend_entries
        .get_mut(&run)
        .unwrap()
        .get_mut(&operation)
        .unwrap()
        .status = SpendStatus::Released;
    Ok(())
}
pub(crate) fn can_approve(d: &Data, operation: &Operation, actor: &Actor) -> bool {
    d.spend_entries
        .get(&operation.run_id)
        .and_then(|entries| entries.get(&operation.meta.id))
        .is_some_and(|entry| {
            entry.status == SpendStatus::Reserved
                && entry.request_digest == operation.request_digest
                && entry.required_approvers.contains(&actor.id)
        })
}

pub(crate) fn release_unstarted(d: &mut Data, run: Uuid) -> Result<()> {
    let ids = d
        .spend_entries
        .get(&run)
        .into_iter()
        .flat_map(BTreeMap::keys)
        .copied()
        .collect::<Vec<_>>();
    for id in ids {
        release(d, run, id)?;
    }
    Ok(())
}

pub(crate) fn interrupted(d: &mut Data, run: Uuid, operation: Uuid, attempt: Uuid) -> Result<()> {
    if let Some(entry) = d
        .spend_entries
        .get_mut(&run)
        .and_then(|e| e.get_mut(&operation))
    {
        if entry.attempt_id != Some(attempt) {
            return Err(Error::Conflict("spend attempt changed".into()));
        }
        if entry.status == SpendStatus::InFlight {
            entry.status = SpendStatus::Unknown;
        }
    }
    Ok(())
}
