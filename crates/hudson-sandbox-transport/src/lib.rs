//! Pinned public SDK transport foundation. No mutation dispatch or runtime integration.
//! Declared profile verification prevents configuration drift; backend auth still owns authority.
use hudson_core::adapters::sandbox_recovery::{Decision, PendingOperation, Receipt, Step};
use hudson_core::{
    models::Actor,
    sandbox_binding::{Binding, Fence},
    storage::Store,
};
pub use sandbox_client::models::{CommandInput, CreateRequest, DestroyRequest};
use sandbox_client::{requests::GetOperation, Client};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid or noncanonical prepared request")]
    Request,
    #[error("original operation requires reconciliation")]
    Reconcile,
    #[error("operator profile declaration differs from the durable binding")]
    ProfileMismatch,
    #[error(transparent)]
    Store(#[from] hudson_core::Error),
    #[error("observation exceeds caller payload bound")]
    Oversized,
    #[error(transparent)]
    Sdk(#[from] sandbox_client::Error),
    #[error("sandbox storage task could not complete")]
    StoreTask,
}

async fn blocking_store<T, F>(operation: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce() -> hudson_core::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| Error::StoreTask)?
        .map_err(Error::Store)
}

/// Canonical SDK wire body; no credentials or inferred package/command mapping.
pub enum PreparedBody {
    Create(CreateRequest),
    Execute(CommandInput),
    Destroy(DestroyRequest),
}
impl PreparedBody {
    pub fn step(&self) -> Step {
        match self {
            Self::Create(_) => Step::Create,
            Self::Execute(_) => Step::Execute,
            Self::Destroy(_) => Step::Destroy,
        }
    }
    /// Persist these exact bytes before a future authorized SDK admission call.
    pub fn canonical_json(&self) -> Result<String, Error> {
        let body = match self {
            Self::Create(value) => serde_json::to_string(value),
            Self::Execute(value) => serde_json::to_string(value),
            Self::Destroy(value) => serde_json::to_string(value),
        }
        .map_err(|_| Error::Request)?;
        if body.len() > 65536 {
            return Err(Error::Request);
        }
        Ok(body)
    }
    /// Reject whitespace/default/unknown-field normalization of existing durable bytes.
    pub fn from_canonical_json(step: Step, body: &str) -> Result<Self, Error> {
        if body.len() > 65536 {
            return Err(Error::Request);
        }
        let parsed = match step {
            Step::Create => Self::Create(serde_json::from_str(body).map_err(|_| Error::Request)?),
            Step::Execute => Self::Execute(serde_json::from_str(body).map_err(|_| Error::Request)?),
            Step::Destroy => Self::Destroy(serde_json::from_str(body).map_err(|_| Error::Request)?),
        };
        if parsed.canonical_json()? != body {
            return Err(Error::Request);
        }
        Ok(parsed)
    }
}

/// SDK protocol probe and owned receipt observer. Profile declaration is not authorization.
/// Unverified probes never establish durable completion or cleanup evidence.
pub struct QualificationObserver {
    client: Client,
}
/// Deliberately no Debug: backend result/error values may contain workload secrets.
pub struct ProbeObservation {
    pub decision: Decision,
    pub result: Option<serde_json::Value>,
    pub error: Option<serde_json::Value>,
    pub output_status: Option<String>,
}
/// An owned Store receipt recorded after matching the constructed client declaration.
/// This establishes operation status only, never guest policy or agent task success.
pub struct BoundObservation {
    pub binding: Binding,
    pub observation: ProbeObservation,
}
impl QualificationObserver {
    /// Reads trusted private SDK configuration; no agent arguments or insecure TLS switch.
    pub fn from_operator_profile(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            client: Client::from_config(path)?,
        })
    }
    /// Canonical representation: SHA256 of UTF-8 serde JSON array [origin, project_id].
    /// Origin uses the SDK's validated URL normalization; no token/CA/path is included.
    /// This pins a trusted local declaration, not server authorization evidence.
    pub fn profile_fingerprint(&self) -> Result<String, Error> {
        let identity = self.client.identity();
        let bytes = serde_json::to_vec(&[identity.origin, identity.project_id])
            .map_err(|_| Error::Request)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
    /// Persist SDK-canonical bytes with the actual constructed client's declaration.
    /// No external request is sent and no admission intent is consumed.
    pub fn prepare_request(
        &self,
        store: &Store,
        actor: &Actor,
        fence: Fence,
        pending: PendingOperation,
        body: &PreparedBody,
    ) -> Result<Binding, Error> {
        if pending.step != body.step() {
            return Err(Error::Request);
        }
        Ok(store.prepare_sandbox_request(
            actor,
            fence,
            pending,
            body.canonical_json()?,
            &self.profile_fingerprint()?,
        )?)
    }
    /// Check owned binding and actual declared profile BEFORE any GET, then fence the Store write.
    /// Backend bearer checks still establish actual project authorization.
    pub async fn observe_binding(
        &self,
        store: &Store,
        actor: &Actor,
        fence: &Fence,
        step: Step,
        max_payload_bytes: usize,
    ) -> Result<BoundObservation, Error> {
        let read_store = store.clone();
        let read_actor = actor.clone();
        let read_fence = fence.clone();
        let read_step = step.clone();
        let binding = blocking_store(move || {
            read_store.sandbox_observation_binding(&read_actor, &read_fence, read_step)
        })
        .await?;
        if binding.profile_fingerprint != self.profile_fingerprint()? {
            return Err(Error::ProfileMismatch);
        }
        let (receipt, observation) = self
            .fetch_receipt(&binding.pending, max_payload_bytes)
            .await?;
        if observation.decision == Decision::Reconcile {
            return Err(Error::Reconcile);
        }
        let write_store = store.clone();
        let write_actor = actor.clone();
        let write_fence = fence.clone();
        let binding = blocking_store(move || {
            write_store.record_sandbox_receipt(&write_actor, &write_fence, step, &receipt)
        })
        .await?;
        Ok(BoundObservation {
            binding,
            observation,
        })
    }
    /// Inspects only the original bound ID. Never waits, cancels, retries, or mutates.
    /// This probe intentionally bypasses durable profile verification; its output is non-authoritative.
    pub async fn inspect_unverified(
        &self,
        pending: &PendingOperation,
        max_payload_bytes: usize,
    ) -> Result<ProbeObservation, Error> {
        Ok(self.fetch_receipt(pending, max_payload_bytes).await?.1)
    }
    async fn fetch_receipt(
        &self,
        pending: &PendingOperation,
        max_payload_bytes: usize,
    ) -> Result<(Receipt, ProbeObservation), Error> {
        if !(1..=65536).contains(&max_payload_bytes) {
            return Err(Error::Request);
        }
        let Decision::Inspect { operation_id } = pending.decision(None) else {
            return Err(Error::Reconcile);
        };
        let operation = self
            .client
            .get_operation(GetOperation {
                operation_id: &operation_id,
            })
            .await?;
        if serde_json::to_vec(&operation)
            .map_err(|_| Error::Request)?
            .len()
            > max_payload_bytes
        {
            return Err(Error::Oversized);
        }
        let receipt = Receipt {
            operation_id: operation.operation_id,
            sandbox_id: operation.sandbox_id,
            kind: operation.kind,
            status: operation.status,
            response_expired: operation.response_expired,
        };
        let decision = pending.decision(Some(&receipt));
        // Unknown/expired/mismatched receipts cannot smuggle an apparent success result.
        let release = matches!(decision, Decision::Succeeded | Decision::Failed);
        Ok((
            receipt,
            ProbeObservation {
                decision,
                result: if release { operation.result } else { None },
                error: if release { operation.error } else { None },
                output_status: operation.output_status,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "current_thread")]
    async fn blocked_storage_does_not_stall_the_async_executor() {
        let (release, blocked) = std::sync::mpsc::channel();
        let storage = blocking_store(move || {
            blocked
                .recv_timeout(std::time::Duration::from_secs(2))
                .map_err(|_| hudson_core::Error::Invalid("async executor stalled".into()))
        });
        let progress = async move {
            tokio::task::yield_now().await;
            release.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(storage, progress);
        result.unwrap();
    }
    #[test]
    fn canonical_bytes_preserve_absolute_deadline_and_reject_normalization() {
        let request = PreparedBody::Execute(CommandInput {
            argv: vec!["/approved/agent".into()],
            env: Default::default(),
            cwd: "/workspace".into(),
            deadline_unix_ms: 123456789,
            output_limit: 4096,
        });
        let body = request.canonical_json().unwrap();
        assert!(body.contains("123456789"));
        assert_eq!(
            PreparedBody::from_canonical_json(Step::Execute, &body)
                .unwrap()
                .canonical_json()
                .unwrap(),
            body
        );
        assert!(PreparedBody::from_canonical_json(Step::Execute, &format!(" {body}")).is_err());
        assert!(PreparedBody::from_canonical_json(Step::Destroy, "{}").is_err());
        assert!(PreparedBody::from_canonical_json(Step::Create, "{\"unknown\":true}").is_err());
    }
}
