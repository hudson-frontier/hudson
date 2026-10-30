use crate::{
    adapters::{
        models::ModelExecutor,
        tools::{ExecutionError, Invocation, ToolExecutor},
    },
    definitions,
    models::*,
    runtime::{bounded, policy_for, tool_error, Runtime},
    security::{admit, Admission},
    Error, Result,
};
use hudson_harness::{Backend, ToolOutcome, ToolResult};
use serde_json::json;
use uuid::Uuid;

impl<B: Backend, M: ModelExecutor, T: ToolExecutor> Runtime<B, M, T> {
    pub(crate) fn dispatch(
        &mut self,
        actor: &Actor,
        run_id: Uuid,
        id: Uuid,
        at: u64,
    ) -> Result<()> {
        let admitted = self.store.transact(|d| {
            let run = d.run(actor, run_id)?.clone();
            let operation = d.operations.get(&id).ok_or(Error::NotFound)?;
            if operation.run_id != run_id {
                return Err(Error::Denied);
            }
            if run.status.terminal() || run.status == RunStatus::Cancelling {
                return Ok(None);
            }
            // A peer can settle and collect this owned operation after tick's
            // status snapshot. It is a no-op; never admit it for replay.
            if !matches!(
                operation.status,
                OperationStatus::Pending | OperationStatus::WaitingApproval
            ) {
                return Ok(None);
            }
            let mut operation = operation.clone();
            if !run.pending_operations.contains(&id) {
                return Err(Error::Denied);
            }
            if operation.request_digest
                != definitions::digest(&(id, run_id, &actor.workspace_id, &operation.request))?
            {
                return Err(Error::Invalid("operation request changed".into()));
            }
            if matches!(operation.request,OperationRequest::Model {..}) {
                match crate::spending::admit(d,&run,&operation,at)? {
                    Admission::Allow=>{},
                    Admission::Deny=>{
                        crate::spending::release(d,run_id,id)?;
                        operation.status=OperationStatus::Denied;
                        operation.revision+=1;
                        d.operations.insert(id,operation);
                        d.event(run_id,Some(id),EventType::OperationSettled,None,json!({"status":"denied","code":"spend_allocation_denied"}));
                        return Ok(None);
                    },
                    Admission::Wait=>{
                        let new_wait=operation.status!=OperationStatus::WaitingApproval;
                        operation.status=OperationStatus::WaitingApproval;
                        operation.approval.get_or_insert_with(||Approval {request_digest:operation.request_digest.clone(),decisions:vec![]});
                        operation.revision+=1;
                        d.operations.insert(id,operation);
                        let run=d.runs.get_mut(&run_id).unwrap();
                        run.status=RunStatus::Waiting;
                        run.wait=Some(WaitReason::Approval {operation_id:id});
                        run.revision+=1;
                        if new_wait {d.event(run_id,Some(id),EventType::ApprovalRequested,None,json!({"kind":"model_spend"}));d.event(run_id,None,EventType::RunWaiting,None,json!({}));}
                        return Ok(None);
                    },
                }
            }
            let tool = match &operation.request {
                OperationRequest::Tool { tool_ref, call } => {
                    let tool = d.tools[&(actor.workspace_id.clone(), tool_ref.clone())].clone();
                    let admission = admit(policy_for(d, &tool), &run.actor_id, &operation, at);
                    // Denial takes precedence over argument feedback. A malformed call
                    // never needs approval and must never reach an executor.
                    if !matches!(admission, Admission::Deny)
                        && definitions::validate(&tool.input_schema, &call.arguments).is_err()
                    {
                        operation.status = OperationStatus::Failed;
                        operation.result = tool_error(
                            &operation,
                            "invalid_arguments",
                            "Arguments do not match the tool's input schema. Correct the arguments before calling this tool again.",
                        );
                        operation.revision += 1;
                        d.operations.insert(id, operation);
                        d.event(
                            run_id,
                            Some(id),
                            EventType::OperationSettled,
                            None,
                            json!({"status": "failed", "code": "invalid_arguments"}),
                        );
                        return Ok(None);
                    }
                    match admission {
                        Admission::Deny => {
                            operation.status = OperationStatus::Denied;
                            operation.result =
                                tool_error(&operation, "denied", "Current policy denied this tool");
                            operation.revision += 1;
                            d.operations.insert(id, operation);
                            d.event(
                                run_id,
                                Some(id),
                                EventType::OperationSettled,
                                None,
                                json!({"status": "denied"}),
                            );
                            return Ok(None);
                        }
                        Admission::Wait => {
                            let new_wait = operation.status != OperationStatus::WaitingApproval;
                            operation.status = OperationStatus::WaitingApproval;
                            operation.approval.get_or_insert_with(|| Approval {
                                request_digest: operation.request_digest.clone(),
                                decisions: vec![],
                            });
                            operation.revision += 1;
                            d.operations.insert(id, operation);
                            let run = d.runs.get_mut(&run_id).expect("exists");
                            run.status = RunStatus::Waiting;
                            run.wait = Some(WaitReason::Approval { operation_id: id });
                            run.revision += 1;
                            if new_wait {
                                d.event(
                                    run_id,
                                    Some(id),
                                    EventType::ApprovalRequested,
                                    None,
                                    json!({}),
                                );
                                d.event(run_id, None, EventType::RunWaiting, None, json!({}));
                            }
                            return Ok(None);
                        }
                        Admission::Allow => {}
                    }
                    if let Err(error) = crate::adapters::sandbox::ensure_supported(&tool.execution)
                    {
                        operation.status = OperationStatus::Failed;
                        operation.result =
                            tool_error(&operation, "unsupported", &error.to_string());
                        operation.revision += 1;
                        d.operations.insert(id, operation);
                        d.event(
                            run_id,
                            Some(id),
                            EventType::OperationSettled,
                            None,
                            json!({"status": "unsupported"}),
                        );
                        return Ok(None);
                    }
                    Some(tool)
                }
                _ => None,
            };
            let attempt_id = Uuid::new_v4();
            if matches!(operation.request,OperationRequest::Model {..}) {crate::spending::start(d,run_id,id,attempt_id);}
            operation.attempts.push(Attempt {
                token_usage: None,
                id: attempt_id,
                number: operation.attempts.len() as u32 + 1,
                started_at: at,
                finished_at: None,
                status: OperationStatus::Running,
                error: None,
            });
            operation.status = OperationStatus::Running;
            operation.revision += 1;
            d.operations.insert(id, operation.clone());
            d.event(
                run_id,
                Some(id),
                EventType::OperationStarted,
                None,
                json!({"attempt_id": attempt_id}),
            );
            Ok(Some((operation, tool, run.limits, attempt_id)))
        })?;
        let Some((operation, tool, limits, attempt_id)) = admitted else {
            return Ok(());
        };

        // The store lock is released before any external execution.
        let result = match &operation.request {
            OperationRequest::Model { request } => {
                let mut observer = crate::model_events::RunModelObserver::new(
                    &self.store,
                    actor,
                    crate::adapters::models::ModelCallContext {
                        run_id,
                        operation_id: id,
                        attempt_id,
                    },
                    limits.max_payload_bytes,
                );
                let result = self.model.call_observed(request, &mut observer);
                observer
                    .finish()
                    .and(result)
                    .map(|response| OperationResult::Model { response })
            }
            OperationRequest::Tool { call, .. } => self
                .tools
                .execute(Invocation {
                    operation_id: id,
                    tool: tool.as_ref().expect("admitted tool"),
                    arguments: &call.arguments,
                })
                .and_then(|value| {
                    if let Some(schema) = &tool.as_ref().expect("tool").output_schema {
                        definitions::validate(schema, &value).map_err(|_| {
                            invalid_result(tool.as_ref(), "tool output schema failed")
                        })?;
                    }
                    Ok(OperationResult::Tool {
                        result: ToolResult {
                            call_id: call.call_id.clone(),
                            outcome: ToolOutcome::Success { value },
                        },
                    })
                }),
            OperationRequest::Verify { candidate } => {
                let (schema, goal) = self.store.read(|d| {
                    let run = d.run(actor, run_id)?;
                    Ok((
                        d.agents[&(actor.workspace_id.clone(), run.agent_ref.clone())]
                            .output_schema
                            .clone(),
                        run.goal.clone(),
                    ))
                })?;
                let source_operations = self.store.operations(actor, run_id)?;
                let mut assessment = crate::verification::check(schema.as_ref(), candidate);
                if assessment.passed {
                    if let Some(goal) = goal {
                        assessment =
                            crate::verification::check(Some(&goal.success_schema), candidate);
                        assessment.feedback = if assessment.passed {
                            "goal output contract passed"
                        } else {
                            "goal output did not satisfy its success schema"
                        }
                        .into();
                        if assessment.passed {
                            if let Some(index) = goal.criteria.iter().position(|rule| {
                                !rule.matches_with_operations(candidate, &source_operations)
                            }) {
                                assessment.passed = false;
                                assessment.feedback =
                                    format!("success criterion {} failed", index + 1);
                            }
                        }
                    }
                }
                let mut evidence = Vec::new();
                for operation in source_operations {
                    if operation.status != OperationStatus::Succeeded {
                        continue;
                    }
                    if let (
                        OperationRequest::Tool { call, .. },
                        Some(OperationResult::Tool { result }),
                    ) = (&operation.request, &operation.result)
                    {
                        if matches!(result.outcome, ToolOutcome::Success { .. }) {
                            evidence.push(Evidence {
                                operation_id: operation.meta.id,
                                tool_name: call.name.clone(),
                                request_digest: operation.request_digest.clone(),
                                result_digest: definitions::digest(result)?,
                            });
                        }
                    }
                }
                Ok(OperationResult::Verify {
                    evidence,
                    passed: assessment.passed,
                    feedback: assessment.feedback,
                })
            }
        }
        .and_then(|result| {
            bounded(&result, limits.max_payload_bytes)
                .map_err(|_| invalid_result(tool.as_ref(), "execution result exceeds limit"))?;
            Ok(result)
        });

        let token_usage = if matches!(operation.request, OperationRequest::Model { .. }) {
            self.model.take_usage()
        } else {
            None
        };
        self.store.transact(|d| {
            let mut op = d.operations.get(&id).cloned().ok_or(Error::NotFound)?;
            if op.status != OperationStatus::Running
                || op.attempts.last().map(|a| a.id) != Some(attempt_id)
            {
                return Err(Error::Conflict("stale execution result".into()));
            }
            let result = if matches!(op.request, OperationRequest::Model { .. })
                && crate::spending::settle(d, run_id, id, attempt_id, token_usage.as_ref())?
            {
                Err(ExecutionError::Failed(
                    "priced model usage exceeded its allocation; spend scopes are frozen".into(),
                ))
            } else {
                result
            };
            let (status, error) = match result {
                Ok(result) => {
                    op.result = Some(result);
                    (OperationStatus::Succeeded, None)
                }
                Err(ExecutionError::Unknown(message)) => (OperationStatus::Unknown, Some(message)),
                Err(ExecutionError::Failed(message)) => {
                    op.result = tool_error(&op, "execution_failed", "Tool execution failed");
                    (OperationStatus::Failed, Some(message))
                }
            };
            op.status = status;
            op.revision += 1;
            let attempt = op.attempts.last_mut().expect("admitted attempt");
            attempt.status = status;
            attempt.token_usage = token_usage.clone();
            if let Some(usage) = &token_usage {
                let run = d.runs.get_mut(&run_id).expect("admitted run");
                run.usage.reported_model_calls = run.usage.reported_model_calls.saturating_add(1);
                run.usage.reported_input_tokens = run
                    .usage
                    .reported_input_tokens
                    .saturating_add(usage.input_tokens);
                run.usage.reported_output_tokens = run
                    .usage
                    .reported_output_tokens
                    .saturating_add(usage.output_tokens);
                run.revision += 1;
            }
            attempt.error = error;
            attempt.finished_at = Some(now());
            let verification = matches!(op.request, OperationRequest::Verify { .. });
            d.operations.insert(id, op);
            d.event(
                run_id,
                Some(id),
                if status == OperationStatus::Unknown {
                    EventType::OperationUnknown
                } else {
                    EventType::OperationSettled
                },
                None,
                json!({"status": status, "token_usage": token_usage}),
            );
            if verification {
                d.event(
                    run_id,
                    Some(id),
                    EventType::AssessmentRecorded,
                    None,
                    json!({}),
                );
            }
            Ok(())
        })
    }
}

/// Losing a write receipt is not evidence that the write did not happen.
fn invalid_result(tool: Option<&Tool>, message: &str) -> ExecutionError {
    if tool.is_some_and(|tool| tool.effect == Effect::Write) {
        ExecutionError::Unknown(message.into())
    } else {
        ExecutionError::Failed(message.into())
    }
}

#[cfg(all(test, feature = "fixtures"))]
mod dispatch_race_tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn stopped_runs_validate_operation_identity_before_noop() {
        for status in [
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::Cancelling,
        ] {
            let mut runtime = fixtures::runtime().unwrap();
            let actor = fixtures::actor();
            let run = runtime
                .submit(
                    &actor,
                    fixtures::agent_ref(),
                    json!({"order_id":"123","action":"lookup"}),
                    None,
                )
                .unwrap();
            runtime.tick(&actor, run).unwrap();
            let owned = runtime.store.operations(&actor, run).unwrap()[0].meta.id;
            let other = runtime
                .submit(
                    &actor,
                    fixtures::agent_ref(),
                    json!({"order_id":"456","action":"lookup"}),
                    None,
                )
                .unwrap();
            runtime.tick(&actor, other).unwrap();
            let foreign = runtime.store.operations(&actor, other).unwrap()[0].meta.id;
            runtime
                .store
                .transact(|data| {
                    data.runs.get_mut(&run).unwrap().status = status;
                    Ok(())
                })
                .unwrap();
            let before = runtime.store.read(definitions::digest).unwrap();
            assert!(
                matches!(
                    runtime.dispatch(&actor, run, foreign, 0),
                    Err(Error::Denied)
                ),
                "{status:?} accepted a foreign operation"
            );
            assert!(
                matches!(
                    runtime.dispatch(&actor, run, Uuid::new_v4(), 0),
                    Err(Error::NotFound)
                ),
                "{status:?} accepted a missing operation"
            );
            runtime.dispatch(&actor, run, owned, 0).unwrap();
            assert_eq!(
                runtime.store.read(definitions::digest).unwrap(),
                before,
                "{status:?} dispatch changed durable state"
            );
        }
    }

    #[test]
    fn a_settled_owned_operation_consumed_by_another_driver_is_a_noop() {
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
        let operation = runtime.store.operations(&actor, id).unwrap()[0].meta.id;
        runtime.tick(&actor, id).unwrap();
        let before = runtime.store.operations(&actor, id).unwrap();
        assert!(!runtime.store.inspect(&actor, id).unwrap().status.terminal());
        assert_eq!(before[0].status, OperationStatus::Succeeded);
        // A peer read Pending before this driver's tick settled and collected
        // the batch, then arrived at dispatch with its old operation ID.
        assert!(runtime.dispatch(&actor, id, operation, 0).is_ok());
        let after = runtime.store.operations(&actor, id).unwrap();
        assert_eq!(after[0].attempts.len(), before[0].attempts.len());
        assert_eq!(after[0].status, OperationStatus::Succeeded);
        let other = runtime
            .submit(
                &actor,
                fixtures::agent_ref(),
                json!({"order_id":"456","action":"lookup"}),
                None,
            )
            .unwrap();
        assert!(matches!(
            runtime.dispatch(&actor, other, operation, 0),
            Err(Error::Denied)
        ));
    }

    #[test]
    fn an_unattached_pending_operation_is_still_denied() {
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
        let operation = runtime.store.operations(&actor, id).unwrap()[0].meta.id;
        for status in [OperationStatus::Pending, OperationStatus::WaitingApproval] {
            runtime
                .store
                .transact(|data| {
                    data.runs.get_mut(&id).unwrap().pending_operations.clear();
                    data.operations.get_mut(&operation).unwrap().status = status;
                    Ok(())
                })
                .unwrap();
            assert!(matches!(
                runtime.dispatch(&actor, id, operation, 0),
                Err(Error::Denied)
            ));
        }
        assert!(runtime.store.operations(&actor, id).unwrap()[0]
            .attempts
            .is_empty());
    }
}
