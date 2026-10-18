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
            if run.status.terminal() || run.status == RunStatus::Cancelling {
                return Ok(None);
            }
            let mut operation = d.operations.get(&id).cloned().ok_or(Error::NotFound)?;
            if operation.run_id != run_id || !run.pending_operations.contains(&id) {
                return Err(Error::Denied);
            }
            if !matches!(
                operation.status,
                OperationStatus::Pending | OperationStatus::WaitingApproval
            ) {
                return Ok(None);
            }
            if operation.request_digest
                != definitions::digest(&(id, run_id, &actor.workspace_id, &operation.request))?
            {
                return Err(Error::Invalid("operation request changed".into()));
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
            OperationRequest::Model { request } => self
                .model
                .call(request)
                .map(|response| OperationResult::Model { response }),
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
