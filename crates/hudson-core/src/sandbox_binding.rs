//! Durable storage protocol for future sandbox transport. No method sends requests.
//! Production Sandbox dispatch remains unsupported; binding requires an admitted attempt.
use crate::{
    adapters::sandbox_recovery::{Admission, Decision, PendingOperation, Receipt},
    definitions,
    models::*,
    storage::{memory::Data, Store},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Fence {
    pub run_id: Uuid,
    pub operation_id: Uuid,
    pub attempt_id: Uuid,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Binding {
    pub fence: Fence,
    pub operation_request_digest: String,
    /// Exact UTF-8 JSON bytes, including the caller's absolute deadline.
    pub body: String,
    /// SHA256 of canonical trusted operator origin+project, excluding credentials.
    pub profile_fingerprint: String,
    pub pending: PendingOperation,
    pub initial_pending: PendingOperation,
    pub terminal: Option<String>,
}

fn key(fence: &Fence, pending: &PendingOperation) -> String {
    format!("{}:{:?}", fence.operation_id, pending.step)
}
fn authorized(d: &Data, actor: &Actor, fence: &Fence, sending: bool) -> Result<String> {
    let run = d.run(actor, fence.run_id)?;
    let op = d
        .operations
        .get(&fence.operation_id)
        .ok_or(Error::NotFound)?;
    if op.run_id != fence.run_id
        || op.meta.workspace_id != actor.workspace_id
        || !run.pending_operations.contains(&fence.operation_id)
    {
        return Err(Error::Denied);
    }
    let OperationRequest::Tool { tool_ref, .. } = &op.request else {
        return Err(Error::Denied);
    };
    let tool = d
        .tools
        .get(&(actor.workspace_id.clone(), tool_ref.clone()))
        .ok_or(Error::NotFound)?;
    if !matches!(tool.execution, Execution::Sandbox { .. }) {
        return Err(Error::Denied);
    }
    if op.request_digest
        != definitions::digest(&(
            fence.operation_id,
            fence.run_id,
            &actor.workspace_id,
            &op.request,
        ))?
    {
        return Err(Error::Conflict("operation request changed".into()));
    }
    let attempt = op.attempts.last().ok_or(Error::Denied)?;
    if attempt.id != fence.attempt_id
        || attempt.status != op.status
        || !matches!(
            op.status,
            OperationStatus::Running | OperationStatus::Unknown
        )
        || (sending
            && (op.status != OperationStatus::Running
                || attempt.finished_at.is_some()
                || run.status.terminal()
                || run.status == RunStatus::Cancelling))
    {
        return Err(Error::Conflict("sandbox attempt fenced".into()));
    }
    Ok(op.request_digest.clone())
}
impl Store {
    /// Persist before any transport send. Identical retries preserve the original bytes.
    /// The trusted worker constructor must derive `profile_fingerprint` from canonical
    /// operator origin+project (never token or agent label). This store cannot verify
    /// provenance; future transport must compare it with the actual SDK client identity.
    pub fn prepare_sandbox_request(
        &self,
        actor: &Actor,
        fence: Fence,
        pending: PendingOperation,
        body: String,
        profile_fingerprint: &str,
    ) -> Result<Binding> {
        self.transact(|d| {
            let operation_request_digest = authorized(d, actor, &fence, true)?;
            if profile_fingerprint.len() != 64
                || !profile_fingerprint
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(Error::Invalid("invalid trusted profile fingerprint".into()));
            }
            if pending.run_id != fence.run_id.to_string()
                || pending.workspace_id != actor.workspace_id
                || pending.admission_attempted
                || pending.operation_id.is_some()
                || pending.decision(None) != Decision::Admit
            {
                return Err(Error::Invalid("invalid initial sandbox request".into()));
            }
            if body.len() > d.runs[&fence.run_id].limits.max_payload_bytes {
                return Err(Error::Invalid(
                    "sandbox body exceeds run payload limit".into(),
                ));
            }
            // Validate bounded bytes without allocating a second copy of their JSON tree.
            let _: serde::de::IgnoredAny = serde_json::from_str(&body)?;
            let id = key(&fence, &pending);
            if let Some(existing) = d.sandbox_bindings.get(&id) {
                if existing.fence == fence
                    && existing.operation_request_digest == operation_request_digest
                    && existing.initial_pending == pending
                    && existing.body == body
                    && existing.profile_fingerprint == profile_fingerprint
                {
                    return Ok(existing.clone());
                }
                return Err(Error::Conflict("sandbox request is immutable".into()));
            }
            if d.sandbox_bindings.values().any(|b| {
                b.profile_fingerprint == profile_fingerprint
                    && b.pending.idempotency_key == pending.idempotency_key
            }) {
                return Err(Error::Conflict(
                    "sandbox key already bound to backend profile".into(),
                ));
            }
            let binding = Binding {
                fence,
                operation_request_digest,
                body,
                profile_fingerprint: profile_fingerprint.into(),
                initial_pending: pending.clone(),
                pending,
                terminal: None,
            };
            d.sandbox_bindings.insert(id, binding.clone());
            Ok(binding)
        })
    }
    /// Atomically consume the single send intent. Repeated calls never authorize a send.
    pub fn begin_sandbox_admission(
        &self,
        actor: &Actor,
        fence: &Fence,
        step: crate::adapters::sandbox_recovery::Step,
    ) -> Result<Binding> {
        self.transact(|d| {
            let current_digest = authorized(d, actor, fence, true)?;
            let id = format!("{}:{step:?}", fence.operation_id);
            let binding = d.sandbox_bindings.get_mut(&id).ok_or(Error::NotFound)?;
            if &binding.fence != fence
                || binding.operation_request_digest != current_digest
                || binding.pending.admission_attempted
            {
                return Err(Error::Conflict(
                    "sandbox admission already attempted or fenced".into(),
                ));
            }
            binding.pending.admission_attempted = true;
            Ok(binding.clone())
        })
    }
    pub fn sandbox_binding(
        &self,
        actor: &Actor,
        fence: &Fence,
        step: crate::adapters::sandbox_recovery::Step,
    ) -> Result<Binding> {
        self.read(|d| {
            d.run(actor, fence.run_id)?;
            let binding = d
                .sandbox_bindings
                .get(&format!("{}:{step:?}", fence.operation_id))
                .ok_or(Error::NotFound)?;
            if &binding.fence != fence {
                return Err(Error::Denied);
            }
            Ok(binding.clone())
        })
    }
    /// Authorize receipt IO against the current pending operation and admitted attempt.
    pub fn sandbox_observation_binding(
        &self,
        actor: &Actor,
        fence: &Fence,
        step: crate::adapters::sandbox_recovery::Step,
    ) -> Result<Binding> {
        self.read(|d| {
            let current_digest = authorized(d, actor, fence, false)?;
            let binding = d
                .sandbox_bindings
                .get(&format!("{}:{step:?}", fence.operation_id))
                .ok_or(Error::NotFound)?;
            if &binding.fence != fence || binding.operation_request_digest != current_digest {
                return Err(Error::Denied);
            }
            Ok(binding.clone())
        })
    }
    /// A 202 binds receipt IDs only. It never records success or cleanup completion.
    pub fn bind_sandbox_admission(
        &self,
        actor: &Actor,
        fence: &Fence,
        step: crate::adapters::sandbox_recovery::Step,
        status: u16,
        admission: &Admission,
    ) -> Result<Binding> {
        self.transact(|d| {
            let current_digest = authorized(d, actor, fence, false)?;
            let binding = d
                .sandbox_bindings
                .get_mut(&format!("{}:{step:?}", fence.operation_id))
                .ok_or(Error::NotFound)?;
            if &binding.fence != fence || binding.operation_request_digest != current_digest {
                return Err(Error::Denied);
            }
            binding.pending = binding
                .pending
                .admitted(status, admission)
                .ok_or_else(|| Error::Conflict("invalid sandbox admission".into()))?;
            Ok(binding.clone())
        })
    }
    pub fn record_sandbox_receipt(
        &self,
        actor: &Actor,
        fence: &Fence,
        step: crate::adapters::sandbox_recovery::Step,
        receipt: &Receipt,
    ) -> Result<Binding> {
        self.transact(|d| {
            let current_digest = authorized(d, actor, fence, false)?;
            let binding = d
                .sandbox_bindings
                .get_mut(&format!("{}:{step:?}", fence.operation_id))
                .ok_or(Error::NotFound)?;
            if &binding.fence != fence || binding.operation_request_digest != current_digest {
                return Err(Error::Denied);
            }
            let decision = binding.pending.decision(Some(receipt));
            if decision == Decision::Reconcile {
                return Err(Error::Conflict(
                    "sandbox receipt requires reconciliation".into(),
                ));
            }
            if let Some(terminal) = &binding.terminal {
                if terminal != &receipt.status {
                    return Err(Error::Conflict(
                        "sandbox terminal receipt is immutable".into(),
                    ));
                }
            } else if matches!(decision, Decision::Succeeded | Decision::Failed) {
                binding.terminal = Some(receipt.status.clone());
            }
            Ok(binding.clone())
        })
    }
}

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    use super::*;
    use crate::{adapters::sandbox_recovery::Step, fixtures};
    use serde_json::json;
    fn seeded(store: Store) -> (Store, Actor, Fence, PendingOperation) {
        let mut runtime = fixtures::runtime().unwrap();
        let actor = fixtures::actor();
        let run_id = runtime
            .submit(
                &actor,
                fixtures::agent_ref(),
                json!({"order_id":"123","action":"lookup"}),
                None,
            )
            .unwrap();
        runtime.tick(&actor, run_id).unwrap();
        let operation_id = runtime.store.operations(&actor, run_id).unwrap()[0].meta.id;
        let attempt_id = Uuid::new_v4();
        let mut snapshot = runtime.store.read(|d| Ok(d.clone())).unwrap();
        let tool = snapshot.tools.values_mut().next().unwrap();
        tool.execution = Execution::Sandbox {
            package: "test-only".into(),
        };
        let tool_ref = tool.reference();
        let name = tool.name.clone();
        let op = snapshot.operations.get_mut(&operation_id).unwrap();
        op.request = OperationRequest::Tool {
            tool_ref,
            call: hudson_harness::ToolCall {
                call_id: "fixture-call".into(),
                provider_metadata: serde_json::Value::Null,
                name,
                arguments: json!({}),
            },
        };
        op.request_digest =
            definitions::digest(&(operation_id, run_id, &actor.workspace_id, &op.request)).unwrap();
        snapshot.runs.get_mut(&run_id).unwrap().pending_operations = vec![operation_id];
        op.status = OperationStatus::Running;
        op.attempts.push(Attempt {
            token_usage: None,
            id: attempt_id,
            number: 1,
            started_at: 1,
            finished_at: None,
            status: OperationStatus::Running,
            error: None,
        });
        // Storage protocol fixture: production Sandbox dispatch still fails closed.
        store
            .transact(|d| {
                *d = snapshot;
                Ok(())
            })
            .unwrap();
        let fence = Fence {
            run_id,
            operation_id,
            attempt_id,
        };
        let pending = PendingOperation {
            run_id: run_id.to_string(),
            workspace_id: actor.workspace_id.clone(),
            step: Step::Create,
            idempotency_key: "stable-create-key-001".into(),
            sandbox_id: None,
            operation_id: None,
            admission_attempted: false,
        };
        (store, actor, fence, pending)
    }
    const PROFILE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn prepare(store: &Store, actor: &Actor, fence: &Fence, pending: &PendingOperation) -> Binding {
        store
            .prepare_sandbox_request(
                actor,
                fence.clone(),
                pending.clone(),
                "{ \"deadline\": 123 }".into(),
                PROFILE,
            )
            .unwrap()
    }
    #[test]
    fn bytes_keys_ownership_and_attempts_are_immutable() {
        let (store, actor, fence, pending) = seeded(Store::default());
        prepare(&store, &actor, &fence, &pending);
        assert!(store
            .prepare_sandbox_request(
                &actor,
                fence.clone(),
                pending.clone(),
                "{\"deadline\":123}".into(),
                PROFILE
            )
            .is_err());
        let mut wrong = actor.clone();
        wrong.id.push('x');
        assert!(store.sandbox_binding(&wrong, &fence, Step::Create).is_err());
        let mut stale = fence.clone();
        stale.attempt_id = Uuid::new_v4();
        assert!(store
            .begin_sandbox_admission(&actor, &stale, Step::Create)
            .is_err());
        let mut duplicate = pending.clone();
        duplicate.step = Step::Execute;
        duplicate.sandbox_id = Some("sandbox".into());
        assert!(store
            .prepare_sandbox_request(&actor, fence.clone(), duplicate, "{}".into(), PROFILE)
            .is_err());
        store
            .begin_sandbox_admission(&actor, &fence, Step::Create)
            .unwrap();
        assert!(store
            .begin_sandbox_admission(&actor, &fence, Step::Create)
            .is_err());
        assert!(
            prepare(&store, &actor, &fence, &pending)
                .pending
                .admission_attempted
        );
    }
    #[test]
    fn body_limit_precedes_json_parsing_and_rejection_does_not_bind_key() {
        let (store, actor, fence, pending) = seeded(Store::default());
        store
            .transact(|d| {
                d.runs
                    .get_mut(&fence.run_id)
                    .unwrap()
                    .limits
                    .max_payload_bytes = 16;
                Ok(())
            })
            .unwrap();
        let error = store
            .prepare_sandbox_request(
                &actor,
                fence.clone(),
                pending.clone(),
                "invalid oversized JSON".into(),
                PROFILE,
            )
            .err()
            .unwrap();
        assert!(
            matches!(error, Error::Invalid(message) if message == "sandbox body exceeds run payload limit")
        );
        assert!(store.sandbox_binding(&actor, &fence, Step::Create).is_err());
        assert!(store
            .prepare_sandbox_request(&actor, fence.clone(), pending.clone(), "{".into(), PROFILE,)
            .is_err());
        let body = "{\"v\":[1,true]}";
        assert_eq!(
            store
                .prepare_sandbox_request(&actor, fence, pending, body.into(), PROFILE)
                .unwrap()
                .body,
            body
        );
    }
    #[test]
    fn profile_identity_is_immutable_and_keys_span_shared_workspaces() {
        let (store, actor, fence, pending) = seeded(Store::default());
        prepare(&store, &actor, &fence, &pending);
        let restarted = Store::staging(
            store
                .read(|d| Ok(serde_json::from_value(serde_json::to_value(d)?)?))
                .unwrap(),
        );
        let other_profile = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert!(restarted
            .prepare_sandbox_request(
                &actor,
                fence.clone(),
                pending.clone(),
                "{ \"deadline\": 123 }".into(),
                other_profile
            )
            .is_err());
        let mut other_actor = actor.clone();
        other_actor.workspace_id = "other-workspace".into();
        let other_fence = Fence {
            run_id: Uuid::new_v4(),
            operation_id: Uuid::new_v4(),
            attempt_id: Uuid::new_v4(),
        };
        restarted
            .transact(|d| {
                let mut run = d.runs[&fence.run_id].clone();
                run.meta.id = other_fence.run_id;
                run.meta.workspace_id = other_actor.workspace_id.clone();
                run.pending_operations = vec![other_fence.operation_id];
                let mut op = d.operations[&fence.operation_id].clone();
                op.meta.id = other_fence.operation_id;
                op.meta.workspace_id = other_actor.workspace_id.clone();
                op.run_id = other_fence.run_id;
                op.attempts.last_mut().unwrap().id = other_fence.attempt_id;
                let OperationRequest::Tool { tool_ref, .. } = &op.request else {
                    unreachable!()
                };
                let mut tool = d.tools[&(actor.workspace_id.clone(), tool_ref.clone())].clone();
                tool.workspace_id = other_actor.workspace_id.clone();
                d.tools
                    .insert((other_actor.workspace_id.clone(), tool_ref.clone()), tool);
                op.request_digest = definitions::digest(&(
                    other_fence.operation_id,
                    other_fence.run_id,
                    &other_actor.workspace_id,
                    &op.request,
                ))?;
                d.runs.insert(other_fence.run_id, run);
                d.operations.insert(other_fence.operation_id, op);
                Ok(())
            })
            .unwrap();
        let mut other_pending = pending;
        other_pending.run_id = other_fence.run_id.to_string();
        other_pending.workspace_id = other_actor.workspace_id.clone();
        assert!(restarted
            .prepare_sandbox_request(
                &other_actor,
                other_fence.clone(),
                other_pending.clone(),
                "{}".into(),
                PROFILE
            )
            .is_err());
        assert!(restarted
            .prepare_sandbox_request(
                &other_actor,
                other_fence,
                other_pending,
                "{}".into(),
                other_profile
            )
            .is_ok());
    }
    #[test]
    fn no_authority_from_registered_tool_or_changed_request() {
        let (store, actor, fence, pending) = seeded(Store::default());
        store
            .transact(|d| {
                let op = d.operations.get(&fence.operation_id).unwrap();
                let OperationRequest::Tool { tool_ref, .. } = &op.request else {
                    unreachable!()
                };
                d.tools
                    .get_mut(&(actor.workspace_id.clone(), tool_ref.clone()))
                    .unwrap()
                    .execution = Execution::Registered {
                    key: "fixture".into(),
                };
                Ok(())
            })
            .unwrap();
        assert!(store
            .prepare_sandbox_request(&actor, fence.clone(), pending.clone(), "{}".into(), PROFILE)
            .is_err());
        let (store, actor, fence, pending) = seeded(Store::default());
        store
            .transact(|d| {
                d.operations
                    .get_mut(&fence.operation_id)
                    .unwrap()
                    .request_digest = "tampered".into();
                Ok(())
            })
            .unwrap();
        assert!(store
            .prepare_sandbox_request(&actor, fence, pending, "{}".into(), PROFILE)
            .is_err());
    }
    #[test]
    fn original_operation_digest_fences_joint_request_changes() {
        let (store, actor, fence, pending) = seeded(Store::default());
        prepare(&store, &actor, &fence, &pending);
        store
            .transact(|d| {
                let op = d.operations.get_mut(&fence.operation_id).unwrap();
                let OperationRequest::Tool { call, .. } = &mut op.request else {
                    unreachable!()
                };
                call.arguments = json!({"changed":true});
                op.request_digest = definitions::digest(&(
                    fence.operation_id,
                    fence.run_id,
                    &actor.workspace_id,
                    &op.request,
                ))?;
                Ok(())
            })
            .unwrap();
        assert!(store
            .begin_sandbox_admission(&actor, &fence, Step::Create)
            .is_err());
        assert!(store
            .sandbox_observation_binding(&actor, &fence, Step::Create)
            .is_err());
        assert!(store
            .prepare_sandbox_request(
                &actor,
                fence,
                pending,
                "{ \"deadline\": 123 }".into(),
                PROFILE
            )
            .is_err());
    }
    #[test]
    fn replaced_attempt_cannot_authorize_an_original_receipt_observation() {
        let (store, actor, fence, pending) = seeded(Store::default());
        prepare(&store, &actor, &fence, &pending);
        assert!(store
            .sandbox_observation_binding(&actor, &fence, Step::Create)
            .is_ok());
        store
            .transact(|d| {
                d.operations
                    .get_mut(&fence.operation_id)
                    .unwrap()
                    .attempts
                    .last_mut()
                    .unwrap()
                    .id = Uuid::new_v4();
                Ok(())
            })
            .unwrap();
        // Historical inspection remains available, but it cannot authorize IO.
        assert!(store.sandbox_binding(&actor, &fence, Step::Create).is_ok());
        assert!(store
            .sandbox_observation_binding(&actor, &fence, Step::Create)
            .is_err());
    }
    #[test]
    fn restart_retains_uncertainty_and_terminal_receipt() {
        let (store, actor, fence, pending) = seeded(Store::default());
        prepare(&store, &actor, &fence, &pending);
        store
            .begin_sandbox_admission(&actor, &fence, Step::Create)
            .unwrap();
        let snapshot = store.read(|d| Ok(serde_json::to_vec(d)?)).unwrap();
        let restarted = Store::staging(serde_json::from_slice(&snapshot).unwrap());
        assert_eq!(
            restarted
                .sandbox_binding(&actor, &fence, Step::Create)
                .unwrap()
                .pending
                .decision(None),
            Decision::Reconcile
        );
        let admission = Admission {
            sandbox_id: "sandbox".into(),
            operation_id: "remote-operation".into(),
            status: "succeeded".into(),
            status_url: "/v1/operations/remote-operation".into(),
        };
        assert!(restarted
            .bind_sandbox_admission(&actor, &fence, Step::Create, 409, &admission)
            .is_err());
        assert!(restarted
            .bind_sandbox_admission(&actor, &fence, Step::Create, 202, &admission)
            .unwrap()
            .terminal
            .is_none());
        restarted
            .transact(|d| {
                let op = d.operations.get_mut(&fence.operation_id).unwrap();
                op.status = OperationStatus::Unknown;
                op.attempts.last_mut().unwrap().status = OperationStatus::Unknown;
                Ok(())
            })
            .unwrap();
        assert!(restarted
            .begin_sandbox_admission(&actor, &fence, Step::Create)
            .is_err());
        let mut receipt = Receipt {
            operation_id: "remote-operation".into(),
            sandbox_id: "sandbox".into(),
            kind: "create".into(),
            status: "succeeded".into(),
            response_expired: false,
        };
        restarted
            .record_sandbox_receipt(&actor, &fence, Step::Create, &receipt)
            .unwrap();
        receipt.status = "failed".into();
        assert!(restarted
            .record_sandbox_receipt(&actor, &fence, Step::Create, &receipt)
            .is_err());
        receipt.status = "succeeded".into();
        receipt.response_expired = true;
        assert!(restarted
            .record_sandbox_receipt(&actor, &fence, Step::Create, &receipt)
            .is_err());
    }
    #[test]
    #[ignore = "requires HUDSON_TEST_DATABASE in local PostgreSQL"]
    fn sandbox_binding_survives_postgres_reconnect() {
        let database = std::env::var("HUDSON_TEST_DATABASE").unwrap();
        let namespace = format!("sandbox-binding-{}", Uuid::new_v4());
        let connect = || Store::postgres_local("/tmp", &database, &namespace).unwrap();
        let (store, actor, fence, pending) = seeded(connect());
        prepare(&store, &actor, &fence, &pending);
        store
            .begin_sandbox_admission(&actor, &fence, Step::Create)
            .unwrap();
        drop(store);
        let reopened = connect();
        let binding = reopened
            .sandbox_binding(&actor, &fence, Step::Create)
            .unwrap();
        assert_eq!(binding.body, "{ \"deadline\": 123 }");
        assert_eq!(binding.pending.decision(None), Decision::Reconcile);
        assert!(reopened
            .begin_sandbox_admission(&actor, &fence, Step::Create)
            .is_err());
    }
}
