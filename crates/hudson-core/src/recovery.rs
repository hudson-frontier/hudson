//! Trusted reconciliation entry point. No automatic replay of uncertain effects.
use crate::{
    adapters::{models::ModelExecutor, tools::ToolExecutor},
    definitions,
    models::*,
    runtime::{bounded, Runtime},
    Error, Result,
};
use hudson_harness::{Backend, ToolOutcome, ToolResult};
use serde_json::{json, Value};
use uuid::Uuid;

impl<B: Backend, M: ModelExecutor, T: ToolExecutor> Runtime<B, M, T> {
    /// Give up an unavailable model response after its attempt became uncertain.
    /// This does not assert that the provider failed or refund admission counters.
    /// The next ordinary tick fails the run (or settles an existing cancellation).
    /// Tool operations cannot use this path: they require destination evidence.
    pub fn abandon_model_response(
        &self,
        actor: &Actor,
        operation_id: Uuid,
        evidence: &str,
    ) -> Result<()> {
        if evidence.trim().is_empty() {
            return Err(Error::Invalid(
                "response-abandonment evidence is required".into(),
            ));
        }
        self.store.transact(|d| {
            let operation = d.operations.get(&operation_id).ok_or(Error::NotFound)?;
            let run = d.run(actor, operation.run_id)?;
            bounded(&evidence, run.limits.max_payload_bytes)?;
            if !matches!(operation.request, OperationRequest::Model { .. }) {
                return Err(Error::Unsupported("only a model response may be abandoned".into()));
            }
            if operation.status != OperationStatus::Unknown {
                return Err(Error::Conflict("model attempt must be uncertain before abandonment".into()));
            }
            let run_id = operation.run_id;
            let operation = d.operations.get_mut(&operation_id).expect("validated operation");
            operation.status = OperationStatus::Failed;
            operation.result = None;
            operation.revision += 1;
            // Attempt remains Unknown: the provider's outcome and billing are not inferred.
            let run = d.runs.get_mut(&run_id).expect("validated run");
            if run.status != RunStatus::Cancelling { run.status = RunStatus::Running; }
            run.wait = None;
            run.revision += 1;
            d.event(run_id, Some(operation_id), EventType::OperationSettled, Some(actor.id.clone()),
                json!({"status":"failed","response_abandoned":true,"evidence":evidence,"admission_refunded":false}));
            Ok(())
        })
    }

    /// Trusted adapter API, not exposed to the model or preview HTTP callers.
    /// The caller must independently verify destination evidence.
    pub fn record_reconciled_tool_result(
        &self,
        actor: &Actor,
        operation_id: Uuid,
        value: Value,
        receipt: &str,
    ) -> Result<()> {
        if receipt.trim().is_empty() {
            return Err(Error::Invalid("receipt is required".into()));
        }
        self.store.transact(|d| {
            let mut operation = d
                .operations
                .get(&operation_id)
                .cloned()
                .ok_or(Error::NotFound)?;
            let run = d.run(actor, operation.run_id)?;
            bounded(&value, run.limits.max_payload_bytes)?;
            bounded(&receipt, run.limits.max_payload_bytes)?;
            if operation.status != OperationStatus::Unknown {
                return Err(Error::Conflict("operation is not uncertain".into()));
            }
            let OperationRequest::Tool { tool_ref, call } = &operation.request else {
                return Err(Error::Unsupported(
                    "only tool-result reconciliation is implemented".into(),
                ));
            };
            let tool = &d.tools[&(actor.workspace_id.clone(), tool_ref.clone())];
            if let Some(schema) = &tool.output_schema {
                definitions::validate(schema, &value)?;
            }
            operation.status = OperationStatus::Succeeded;
            operation.result = Some(OperationResult::Tool {
                result: ToolResult {
                    call_id: call.call_id.clone(),
                    outcome: ToolOutcome::Success { value },
                },
            });
            operation.revision += 1;
            let run_id = operation.run_id;
            d.operations.insert(operation_id, operation);
            d.event(
                run_id,
                Some(operation_id),
                EventType::OperationSettled,
                Some(actor.id.clone()),
                json!({"status": "succeeded", "reconciled": true, "receipt": receipt}),
            );
            let run = d.runs.get_mut(&run_id).expect("exists");
            if run.status != RunStatus::Cancelling {
                run.status = RunStatus::Running;
            }
            run.wait = None;
            run.revision += 1;
            Ok(())
        })
    }
}

impl<B: Backend, M: ModelExecutor, T: ToolExecutor> Runtime<B, M, T> {
    /// Trusted operator recovery after independently confirming the executor has
    /// stopped. Age alone is not evidence of death. Fence the exact observed
    /// attempt so a late result cannot overwrite reconciliation.
    pub fn mark_interrupted(
        &self,
        actor: &Actor,
        operation_id: Uuid,
        attempt_id: Uuid,
        evidence: &str,
    ) -> Result<()> {
        if evidence.trim().is_empty() {
            return Err(Error::Invalid("executor-stop evidence is required".into()));
        }
        self.store.transact(|d| {
            let mut operation = d.operations.get(&operation_id).cloned().ok_or(Error::NotFound)?;
            let run = d.run(actor, operation.run_id)?;
            bounded(&evidence, run.limits.max_payload_bytes)?;
            if operation.status != OperationStatus::Running
                || operation.attempts.last().map(|a| a.id) != Some(attempt_id) {
                return Err(Error::Conflict("running attempt changed".into()));
            }
            operation.status = OperationStatus::Unknown;
            crate::spending::interrupted(d,operation.run_id,operation_id,attempt_id)?;
            operation.revision += 1;
            let attempt = operation.attempts.last_mut().expect("validated attempt");
            attempt.status = OperationStatus::Unknown;
            attempt.finished_at = Some(now());
            attempt.error = Some("executor interrupted; outcome requires reconciliation".into());
            let run_id = operation.run_id;
            d.operations.insert(operation_id, operation);
            let run = d.runs.get_mut(&run_id).expect("validated run");
            if run.status != RunStatus::Cancelling { run.status = RunStatus::Waiting; }
            run.wait = Some(WaitReason::Reconciliation { operation_id });
            run.revision += 1;
            d.event(run_id, Some(operation_id), EventType::OperationSettled, Some(actor.id.clone()),
                json!({"status":"unknown","interrupted":true,"attempt_id":attempt_id,"evidence":evidence}));
            Ok(())
        })
    }
}

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    use super::*;
    use crate::fixtures;
    #[test]
    fn interrupted_attempt_is_fenced_and_never_replayed() {
        let mut runtime = fixtures::runtime().unwrap();
        let actor = fixtures::actor();
        let id = runtime
            .submit(
                &actor,
                fixtures::agent_ref(),
                json!({"order_id":"123","action":"lookup"}),
                None,
            )
            .unwrap();
        runtime.tick(&actor, id).unwrap();
        let op_id = runtime.store.operations(&actor, id).unwrap()[0].meta.id;
        let attempt_id = Uuid::new_v4();
        // Reproduce the durable state left after dispatch intent commits and the
        // executor process dies before recording a result.
        runtime
            .store
            .transact(|d| {
                let op = d.operations.get_mut(&op_id).unwrap();
                op.status = OperationStatus::Running;
                op.attempts.push(Attempt {
                    token_usage: None,
                    id: attempt_id,
                    number: 1,
                    started_at: now(),
                    finished_at: None,
                    status: OperationStatus::Running,
                    error: None,
                });
                Ok(())
            })
            .unwrap();
        assert!(runtime
            .mark_interrupted(&actor, op_id, Uuid::new_v4(), "process exited")
            .is_err());
        assert!(runtime
            .mark_interrupted(&actor, op_id, attempt_id, "")
            .is_err());
        runtime
            .mark_interrupted(&actor, op_id, attempt_id, "process exit confirmed")
            .unwrap();
        for _ in 0..3 {
            runtime.tick(&actor, id).unwrap();
        }
        let op = &runtime.store.operations(&actor, id).unwrap()[0];
        assert_eq!(op.status, OperationStatus::Unknown);
        assert_eq!(op.attempts.len(), 1);
        assert_eq!(op.attempts[0].status, OperationStatus::Unknown);
        assert!(matches!(
            runtime.store.inspect(&actor, id).unwrap().wait,
            Some(WaitReason::Reconciliation { .. })
        ));
        assert!(runtime
            .mark_interrupted(&actor, op_id, attempt_id, "repeat")
            .is_err());
        let usage = runtime.store.inspect(&actor, id).unwrap().usage;
        assert!(runtime.abandon_model_response(&actor, op_id, "").is_err());
        runtime
            .abandon_model_response(&actor, op_id, "response unavailable after process exit")
            .unwrap();
        assert_eq!(runtime.tick(&actor, id).unwrap().status, RunStatus::Failed);
        let operation = runtime.store.operations(&actor, id).unwrap().remove(0);
        assert_eq!(operation.status, OperationStatus::Failed);
        assert_eq!(operation.attempts[0].status, OperationStatus::Unknown);
        assert_eq!(runtime.store.inspect(&actor, id).unwrap().usage, usage);
        assert!(runtime
            .abandon_model_response(&actor, op_id, "duplicate")
            .is_err());
    }
    #[test]
    fn abandonment_cannot_settle_an_uncertain_tool() {
        let mut runtime = fixtures::runtime().unwrap();
        runtime.tools.uncertain = true;
        let actor = fixtures::actor();
        let id = runtime
            .submit(
                &actor,
                fixtures::agent_ref(),
                json!({"order_id":"123","action":"lookup"}),
                None,
            )
            .unwrap();
        let waiting = fixtures::drive(&mut runtime, &actor, id).unwrap();
        let Some(WaitReason::Reconciliation { operation_id }) = waiting.wait else {
            panic!("expected uncertainty")
        };
        assert!(runtime
            .abandon_model_response(&actor, operation_id, "not a model")
            .is_err());
        assert_eq!(
            runtime
                .store
                .inspect_operation(&actor, operation_id)
                .unwrap()
                .status,
            OperationStatus::Unknown
        );
    }
}
