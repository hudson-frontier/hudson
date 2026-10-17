use crate::{
    adapters::{models::ModelExecutor, tools::ToolExecutor},
    budgets, definitions,
    models::*,
    storage::{memory::Data, MemoryStore},
    Error, Result,
};
use hudson_harness::{Action, Backend, Engine, Input, ModelResponse, ToolOutcome, ToolResult};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use uuid::Uuid;

pub struct Runtime<B, M, T> {
    pub store: MemoryStore,
    pub(crate) engine: Engine<B>,
    pub(crate) model: M,
    pub(crate) tools: T,
}

impl<B: Backend, M: ModelExecutor, T: ToolExecutor> Runtime<B, M, T> {
    pub fn new(store: MemoryStore, backend: B, model: M, tools: T) -> Self {
        Self {
            store,
            engine: Engine::new(backend),
            model,
            tools,
        }
    }

    pub fn model_budget_binding(&self) -> Option<budgets::ModelBudgetBinding> {
        self.model.budget_binding()
    }

    pub fn submit(
        &self,
        actor: &Actor,
        agent_ref: VersionRef,
        input: Value,
        request_key: Option<String>,
    ) -> Result<Uuid> {
        self.submit_with_goal(actor, agent_ref, input, request_key, None)
    }

    pub fn submit_with_goal(
        &self,
        actor: &Actor,
        agent_ref: VersionRef,
        input: Value,
        request_key: Option<String>,
        goal: Option<Goal>,
    ) -> Result<Uuid> {
        self.submit_scheduled(actor, agent_ref, input, request_key, goal, None)
    }

    /// Commit the run and optional scheduling intent in one store transaction.
    pub fn submit_scheduled(
        &self,
        actor: &Actor,
        agent_ref: VersionRef,
        input: Value,
        request_key: Option<String>,
        goal: Option<Goal>,
        schedule: Option<crate::scheduling::ScheduleTarget>,
    ) -> Result<Uuid> {
        if let Some(target) = &schedule {
            target.validate()?;
        }
        if let Some(goal) = &goal {
            if goal.objective.trim().is_empty() || goal.objective.len() > 16384 {
                return Err(Error::Invalid(
                    "goal requires an objective of at most 16 KiB".into(),
                ));
            }
            definitions::validate_schema(&goal.success_schema)?;
            for criterion in &goal.criteria {
                criterion.validate()?;
            }
        }
        if actor.id.trim().is_empty() || actor.workspace_id.trim().is_empty() {
            return Err(Error::Denied);
        }
        if request_key
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 256)
        {
            return Err(Error::Invalid("request key length".into()));
        }
        let input_digest = match &goal {
            Some(goal) => definitions::digest(&(agent_ref.clone(), &input, goal))?,
            None => definitions::digest(&(agent_ref.clone(), &input))?,
        };
        self.store.transact(|d| {
            let key = request_key
                .as_ref()
                .map(|k| (actor.workspace_id.clone(), actor.id.clone(), k.clone()));
            if let Some(id) = key.as_ref().and_then(|k| d.submissions.get(k)) {
                if d.runs[id].input_digest != input_digest
                    || d.runs[id].model_budget != self.model.budget_binding()
                    || d.schedule_requests.get(id).map(|r| &r.target) != schedule.as_ref()
                {
                    return Err(Error::Conflict(
                        "submission key reused with different input or model budget".into(),
                    ));
                }
                return Ok(*id);
            }
            let agent = d
                .agents
                .get(&(actor.workspace_id.clone(), agent_ref.clone()))
                .ok_or(Error::NotFound)?;
            bounded(&input, agent.limits.max_payload_bytes)?;
            bounded(&goal, agent.limits.max_payload_bytes)?;
            if let Some(schema) = &agent.input_schema {
                definitions::validate(schema, &input)?;
            }
            let meta = Metadata::new(&actor.workspace_id);
            let id = meta.id;
            let run = Run {
                meta,
                agent_ref,
                parent_operation: None,
                model_budget: self.model.budget_binding(),
                actor_id: actor.id.clone(),
                request_key,
                input_digest,
                input: input.clone(),
                goal,
                status: RunStatus::Queued,
                reason: None,
                wait: None,
                state: self.engine.initial_state(),
                pending_operations: vec![],
                next_input: Some(Input::Start { value: input }),
                limits: agent.limits.clone(),
                usage: Usage::default(),
                result: None,
                assessment: None,
                verified_candidate_digest: None,
                revision: 0,
            };
            d.runs.insert(id, run);
            if let Some(target) = schedule {
                d.schedule_requests.insert(
                    id,
                    crate::scheduling::ScheduleRequest {
                        target,
                        acknowledged: false,
                        published_at: None,
                        last_attempt: 0,
                    },
                );
            }
            if let Some(key) = key {
                d.submissions.insert(key, id);
            }
            d.event(
                id,
                None,
                EventType::RunCreated,
                Some(actor.id.clone()),
                json!({}),
            );
            Ok(id)
        })
    }

    /// One bounded local step. A caller can release this Runtime while the run waits.
    pub fn tick(&mut self, actor: &Actor, id: Uuid) -> Result<RunView> {
        self.store.prepare_run_memory(actor, id)?;
        let run = self.store.read(|d| Ok(d.run(actor, id)?.clone()))?;
        if run.status.terminal() {
            return self.store.inspect(actor, id);
        }
        if run.model_budget != self.model.budget_binding() {
            return Err(Error::Conflict("run model budget binding changed".into()));
        }
        if run.status == RunStatus::Cancelling {
            self.finish_cancellation(actor, id)?;
            return self.store.inspect(actor, id);
        }
        if !self.store.dependencies_ready(actor, id)? {
            return self.store.inspect(actor, id);
        }
        if !run.pending_operations.is_empty() {
            for operation_id in &run.pending_operations {
                let status = self.store.read(|d| Ok(d.operations[operation_id].status))?;
                if matches!(
                    status,
                    OperationStatus::Pending | OperationStatus::WaitingApproval
                ) {
                    self.dispatch(actor, id, *operation_id, now())?;
                    break;
                }
                // This first driver is sequential. Another tick/worker must not
                // overtake an active or uncertain member of the same batch.
                if matches!(status, OperationStatus::Running | OperationStatus::Unknown) {
                    break;
                }
            }
            self.collect(actor, id)?;
            // Cancellation can arrive while dispatch performs blocking IO. Settle it
            // after the receipt is persisted, without requiring another host tick.
            let view = self.store.inspect(actor, id)?;
            if view.status == RunStatus::Cancelling {
                self.finish_cancellation(actor, id)?;
                return self.store.inspect(actor, id);
            }
            return Ok(view);
        }
        let Some(input) = run.next_input.clone() else {
            return self.store.inspect(actor, id);
        };
        if run.state.step >= run.limits.max_harness_steps {
            self.fail(actor, id, run.revision, "harness step budget exhausted")?;
            return self.store.inspect(actor, id);
        }
        let (config, dependency_artifacts) = self.store.read(|d| {
            let agent = &d.agents[&(actor.workspace_id.clone(), run.agent_ref.clone())];
            let mut config = crate::adapters::harness::project(agent, d)?;
            if let Some(goal) = &run.goal {
                config.instructions.push_str(&format!(
                    "\nTask objective: {}\nFinal output must satisfy this JSON Schema: {}",
                    goal.objective, goal.success_schema
                ));
                if !goal.criteria.is_empty() {
                    config.instructions.push_str(&format!(
                        "\nAdditional success criteria: {}",
                        serde_json::to_string(&goal.criteria)?
                    ));
                }
            }
            crate::memory::append_snapshot(d, id, &mut config.instructions)?;
            let artifacts =
                crate::coordination::append_dependency_context(d, id, &mut config.instructions)?;
            Ok((config, artifacts))
        })?;
        let context_policy = self.store.read(|d| {
            Ok(d.context_policies
                .get(&(actor.workspace_id.clone(), run.agent_ref.clone()))
                .cloned()
                .flatten())
        })?;
        let prepared = match crate::context::advance(
            &self.engine,
            &config,
            &run.state,
            input.clone(),
            context_policy.as_ref(),
            &actor.workspace_id,
            id,
        ) {
            Ok(t) => t,
            Err(e) => {
                self.fail(actor, id, run.revision, &e.to_string())?;
                return self.store.inspect(actor, id);
            }
        };
        let transition = prepared.transition;
        let committed = self.store.transact(|d| {
            let mut current = d.run(actor, id)?.clone();
            if current.revision != run.revision { return Err(Error::Conflict("stale run revision".into())); }
            crate::context::commit(d, &prepared.artifacts)?;
            crate::context::commit(d, &dependency_artifacts)?;
            bounded(&transition.checkpoint, current.limits.max_context_bytes)?;
            bounded(&transition.action, current.limits.max_context_bytes)?;
            let agent = d.agents[&(actor.workspace_id.clone(), current.agent_ref.clone())].clone();
            let mut requests = Vec::new();
            match transition.action {
                Action::CallModel { ref request } => {
                    if request.model != config.model || request.instructions != config.instructions
                        || request.tools != config.model_tools().map_err(|e| Error::Invalid(e.to_string()))? {
                        return Err(Error::Invalid("model request changed configured model/instructions/tools".into()));
                    }
                    bounded(request, current.limits.max_context_bytes)?;
                    requests.push(OperationRequest::Model { request: request.clone() });
                }
                Action::ExecuteTools { ref calls } => {
                    // Tool requests must preserve the exact completed model response.
                    if !matches!(&input, Input::Model { response: ModelResponse::ToolCalls { calls: expected } }
                        if expected == calls) {
                        return Err(Error::Invalid("tool calls do not match model response".into()));
                    }
                    let mut ids = BTreeSet::new();
                    if calls.is_empty() { return Err(Error::Invalid("empty tool batch".into())); }
                    for call in calls {
                        if call.call_id.trim().is_empty() || !ids.insert(&call.call_id) {
                            return Err(Error::Invalid("duplicate or empty tool-call ID".into()));
                        }
                        let binding = agent.tools.iter().find(|t| t.alias == call.name)
                            .ok_or_else(|| Error::Invalid("unregistered tool requested".into()))?;
                        let tool = &d.tools[&(actor.workspace_id.clone(), binding.tool_ref.clone())];
                        definitions::validate(&tool.input_schema, &call.arguments)?;
                        requests.push(OperationRequest::Tool { tool_ref: binding.tool_ref.clone(), call: call.clone() });
                    }
                }
                Action::Verify { ref candidate } => {
                    if !matches!(&input, Input::Model { response: ModelResponse::Final { output } } if output == candidate) {
                        return Err(Error::Invalid("candidate must match the model's final response".into()));
                    }
                    requests.push(OperationRequest::Verify { candidate: candidate.clone() });
                }
                Action::Complete { ref output } => {
                    if current.verified_candidate_digest.as_ref() != Some(&definitions::digest(output)?) {
                        return Err(Error::Invalid("completion requires a passed check of this exact output".into()));
                    }
                    bounded(output, current.limits.max_payload_bytes)?;
                    current.result = Some(output.clone());
                    current.status = RunStatus::Completed;
                }
                Action::WaitForInput { ref prompt } => {
                    current.status = RunStatus::Waiting;
                    current.wait = Some(WaitReason::UserInput { prompt: prompt.clone(), question_id: transition.checkpoint.step });
                }
                Action::Fail { ref reason } => {
                    current.status = RunStatus::Failed;
                    current.reason = Some(reason.clone());
                }
            }
            budgets::reserve(&mut current, &requests)?;
            for request in &requests { bounded(request, current.limits.max_context_bytes)?; }
            current.state = transition.checkpoint;
            current.next_input = None;
            current.pending_operations.clear();
            current.revision += 1;
            if !requests.is_empty() {
                current.status = RunStatus::Running;
                current.wait = None;
                current.verified_candidate_digest = None;
            }
            let mut new_operations = Vec::new();
            for (index, request) in requests.into_iter().enumerate() {
                let meta = Metadata::new(&actor.workspace_id);
                let digest = definitions::digest(&(meta.id, id, &actor.workspace_id, &request))?;
                current.pending_operations.push(meta.id);
                new_operations.push(Operation { meta, run_id: id, step_index: current.state.step,
                    request_index: index, request, request_digest: digest, status: OperationStatus::Pending,
                    approval: None, attempts: vec![], result: None, revision: 0 });
            }
            let status = current.status;
            crate::memory::retain_completed(d, &current)?;
            d.runs.insert(id, current);
            for operation in new_operations {
                let operation_id = operation.meta.id;
                d.operations.insert(operation_id, operation);
                d.event(id, Some(operation_id), EventType::OperationRequested, None, json!({}));
            }
            if status.terminal() {
                d.event(id, None, EventType::RunFinished, None, json!({"status": status}));
            } else if status == RunStatus::Waiting {
                d.event(id, None, EventType::RunWaiting, None, json!({}));
            }
            Ok(())
        });
        if let Err(error) = committed {
            if matches!(error, Error::Conflict(_)) {
                return Err(error);
            }
            self.fail(actor, id, run.revision, &error.to_string())?;
        }
        self.store.inspect(actor, id)
    }

    fn collect(&self, actor: &Actor, id: Uuid) -> Result<()> {
        self.store.transact(|d| {
            let mut run = d.run(actor, id)?.clone();
            if run.pending_operations.is_empty()
                || run.status.terminal()
                || run.status == RunStatus::Cancelling
            {
                return Ok(());
            }
            let operations: Vec<_> = run
                .pending_operations
                .iter()
                .map(|id| d.operations[id].clone())
                .collect();
            if let Some(op) = operations
                .iter()
                .find(|o| o.status == OperationStatus::Unknown)
            {
                run.status = RunStatus::Waiting;
                run.wait = Some(WaitReason::Reconciliation {
                    operation_id: op.meta.id,
                });
                run.revision += 1;
                d.runs.insert(id, run);
                return Ok(());
            }
            if operations.iter().any(|o| {
                matches!(
                    o.status,
                    OperationStatus::Pending
                        | OperationStatus::WaitingApproval
                        | OperationStatus::Running
                )
            }) {
                return Ok(());
            }
            if run.status == RunStatus::Cancelling {
                return Ok(());
            }
            let next = match &operations[0].request {
                OperationRequest::Model { .. } => match &operations[0].result {
                    Some(OperationResult::Model { response }) => Some(Input::Model {
                        response: response.clone(),
                    }),
                    _ => None,
                },
                OperationRequest::Tool { .. } => {
                    let mut results = Vec::new();
                    for operation in &operations {
                        if let Some(OperationResult::Tool { result }) = &operation.result {
                            results.push(result.clone());
                        } else if let OperationRequest::Tool { call, .. } = &operation.request {
                            results.push(ToolResult {
                                call_id: call.call_id.clone(),
                                outcome: ToolOutcome::Error {
                                    code: "execution_failed".into(),
                                    message: "Tool did not complete".into(),
                                },
                            });
                        }
                    }
                    Some(Input::Tools { results })
                }
                OperationRequest::Verify { candidate } => match &operations[0].result {
                    Some(OperationResult::Verify {
                        passed,
                        feedback,
                        evidence,
                    }) => {
                        run.assessment = Some(Assessment {
                            passed: *passed,
                            feedback: feedback.clone(),
                            evidence: evidence.clone(),
                        });
                        if *passed {
                            run.verified_candidate_digest = Some(definitions::digest(candidate)?);
                        }
                        Some(Input::Verification {
                            passed: *passed,
                            feedback: feedback.clone(),
                        })
                    }
                    _ => None,
                },
            };
            run.pending_operations.clear();
            run.next_input = next;
            run.wait = None;
            run.status = if run.next_input.is_some() {
                RunStatus::Running
            } else {
                RunStatus::Failed
            };
            if run.status == RunStatus::Failed {
                run.reason = Some("operation failed".into());
            }
            run.revision += 1;
            let status = run.status;
            d.runs.insert(id, run);
            if status == RunStatus::Failed {
                d.event(
                    id,
                    None,
                    EventType::RunFinished,
                    None,
                    json!({"status": status}),
                );
            }
            Ok(())
        })
    }

    fn fail(&self, actor: &Actor, id: Uuid, revision: u64, reason: &str) -> Result<()> {
        self.store.transact(|d| {
            let run = d.run(actor, id)?;
            if run.revision != revision {
                return Err(Error::Conflict("stale run revision".into()));
            }
            let run = d.runs.get_mut(&id).expect("checked");
            run.status = RunStatus::Failed;
            run.reason = Some(reason.into());
            run.revision += 1;
            d.event(
                id,
                None,
                EventType::RunFinished,
                None,
                json!({"status": "failed", "reason": reason}),
            );
            Ok(())
        })
    }

    pub fn approve(
        &self,
        approver: &Actor,
        operation_id: Uuid,
        approved: bool,
        at: u64,
        expires_at: u64,
    ) -> Result<()> {
        if expires_at <= at {
            return Err(Error::Invalid("approval already expired".into()));
        }
        self.store.transact(|d| {
            let mut operation = d
                .operations
                .get(&operation_id)
                .cloned()
                .ok_or(Error::NotFound)?;
            crate::security::require_workspace(approver, &operation.meta.workspace_id)?;
            if operation.status != OperationStatus::WaitingApproval {
                return Err(Error::Conflict("operation is not awaiting approval".into()));
            }
            let run = &d.runs[&operation.run_id];
            if run.status.terminal() || run.status == RunStatus::Cancelling {
                return Err(Error::Conflict("run stopped".into()));
            }
            let OperationRequest::Tool { tool_ref, call } = &operation.request else {
                return Err(Error::Invalid("approval requires tool".into()));
            };
            let tool = &d.tools[&(approver.workspace_id.clone(), tool_ref.clone())];
            let policy = d
                .policies
                .get(&(approver.workspace_id.clone(), tool.policy_ref.clone()))
                .ok_or(Error::Denied)?;
            if !policy.approvers.contains(&approver.id) {
                return Err(Error::Denied);
            }
            operation
                .approval
                .as_mut()
                .ok_or_else(|| Error::Invalid("missing approval request".into()))?
                .decisions
                .push(ApprovalDecision {
                    actor_id: approver.id.clone(),
                    request_digest: operation.request_digest.clone(),
                    approved,
                    decided_at: at,
                    expires_at,
                });
            if approved {
                operation.status = OperationStatus::Pending;
            } else {
                operation.status = OperationStatus::Denied;
                operation.result = Some(OperationResult::Tool {
                    result: ToolResult {
                        call_id: call.call_id.clone(),
                        outcome: ToolOutcome::Error {
                            code: "denied".into(),
                            message: "Approval denied".into(),
                        },
                    },
                });
            }
            operation.revision += 1;
            let run_id = operation.run_id;
            d.operations.insert(operation_id, operation);
            let run = d.runs.get_mut(&run_id).expect("exists");
            run.status = RunStatus::Running;
            run.wait = None;
            run.revision += 1;
            d.event(
                run_id,
                Some(operation_id),
                EventType::ApprovalDecided,
                Some(approver.id.clone()),
                json!({"approved": approved}),
            );
            d.event(run_id, None, EventType::RunResumed, None, json!({}));
            Ok(())
        })
    }

    pub fn provide_input(
        &self,
        actor: &Actor,
        id: Uuid,
        question_id: u64,
        key: &str,
        value: Value,
    ) -> Result<()> {
        if key.is_empty() || key.len() > 256 {
            return Err(Error::Invalid("input key length".into()));
        }
        let digest = definitions::digest(&(question_id, &value))?;
        self.store.transact(|d| {
            let run = d.run(actor, id)?;
            bounded(&value, run.limits.max_payload_bytes)?;
            let entry = (id, key.to_owned());
            if let Some(previous) = d.inputs.get(&entry) {
                return if previous == &digest {
                    Ok(())
                } else {
                    Err(Error::Conflict("input key reused".into()))
                };
            }
            if !matches!(run.wait, Some(WaitReason::UserInput { question_id: expected, .. }) if expected == question_id)
                || run.status != RunStatus::Waiting
            {
                return Err(Error::Conflict("run is not waiting for this question".into()));
            }
            let run = d.runs.get_mut(&id).expect("checked");
            run.next_input = Some(Input::User {
                value: value.clone(),
            });
            run.wait = None;
            run.status = RunStatus::Running;
            run.revision += 1;
            d.inputs.insert(entry, digest);
            d.event(
                id,
                None,
                EventType::InputReceived,
                Some(actor.id.clone()),
                json!({"input": value, "question_id": question_id}),
            );
            Ok(())
        })
    }

    pub fn cancel(&self, actor: &Actor, id: Uuid) -> Result<()> {
        let root_run = id;
        let tree = self.store.transact(|d| {
            d.run(actor, id)?;
            let mut tree = vec![id];
            let mut index = 0;
            while index < tree.len() {
                let parent = tree[index];
                let children: Vec<_> = d
                    .runs
                    .values()
                    .filter(|r| {
                        r.parent_operation
                            .and_then(|op| d.operations.get(&op))
                            .is_some_and(|op| op.run_id == parent)
                    })
                    .map(|r| r.meta.id)
                    .collect();
                for child in children {
                    if !tree.contains(&child) {
                        d.run(actor, child)?;
                        tree.push(child);
                    }
                }
                index += 1;
            }
            for id in &tree {
                let run = d.runs.get_mut(id).expect("checked");
                if run.status.terminal() || run.status == RunStatus::Cancelling {
                    continue;
                }
                run.status = RunStatus::Cancelling;
                run.next_input = None;
                run.revision += 1;
                d.event(
                    *id,
                    None,
                    EventType::CancellationRequested,
                    Some(actor.id.clone()),
                    json!({"root_run":root_run}),
                );
            }
            Ok(tree)
        })?;
        for id in tree.into_iter().rev() {
            self.finish_cancellation(actor, id)?;
        }
        Ok(())
    }

    fn finish_cancellation(&self, actor: &Actor, id: Uuid) -> Result<()> {
        self.store.transact(|d| {
            let run = d.run(actor, id)?.clone();
            if run.status != RunStatus::Cancelling {
                return Ok(());
            }
            let mut unresolved = None;
            for op_id in run.pending_operations {
                let op = d
                    .operations
                    .get_mut(&op_id)
                    .expect("pending operation exists");
                match op.status {
                    OperationStatus::Running | OperationStatus::Unknown => unresolved = Some(op_id),
                    OperationStatus::Pending | OperationStatus::WaitingApproval => {
                        op.status = OperationStatus::Cancelled;
                        op.revision += 1;
                    }
                    _ => {}
                }
            }
            for child in d.runs.values() {
                if !child.status.terminal() {
                    if let Some(op_id) = child
                        .parent_operation
                        .filter(|op_id| d.operations.get(op_id).is_some_and(|op| op.run_id == id))
                    {
                        unresolved = Some(op_id);
                    }
                }
            }
            let run = d.runs.get_mut(&id).expect("checked");
            run.revision += 1;
            if let Some(operation_id) = unresolved {
                run.wait = Some(WaitReason::Reconciliation { operation_id });
            } else {
                run.status = RunStatus::Cancelled;
                run.wait = None;
                d.event(
                    id,
                    None,
                    EventType::RunFinished,
                    None,
                    json!({"status": "cancelled"}),
                );
            }
            Ok(())
        })
    }
}

pub(crate) fn bounded(value: &impl serde::Serialize, limit: usize) -> Result<()> {
    if serde_json::to_vec(value)?.len() > limit {
        return Err(Error::Invalid("payload exceeds configured limit".into()));
    }
    Ok(())
}

pub(crate) fn tool_error(
    operation: &Operation,
    code: &str,
    message: &str,
) -> Option<OperationResult> {
    match &operation.request {
        OperationRequest::Tool { call, .. } => Some(OperationResult::Tool {
            result: ToolResult {
                call_id: call.call_id.clone(),
                outcome: ToolOutcome::Error {
                    code: code.into(),
                    message: message.into(),
                },
            },
        }),
        _ => None,
    }
}

pub(crate) fn policy_for<'a>(data: &'a Data, tool: &Tool) -> Option<&'a crate::security::Policy> {
    data.policies
        .get(&(tool.workspace_id.clone(), tool.policy_ref.clone()))
}
