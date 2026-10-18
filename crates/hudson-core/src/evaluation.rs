//! Repeatable cases run through the ordinary runtime, policies, and budgets.
//! Expected schemas are evaluator-only; they are never given to the agent.
use crate::{
    adapters::{models::ModelExecutor, tools::ToolExecutor},
    definitions,
    models::*,
    runtime::Runtime,
    Error, Result,
};
use hudson_harness::Backend;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    pub input: Value,
    pub expected_schema: Value,
    #[serde(default)]
    pub criteria: Vec<crate::verification::Criterion>,
}

#[derive(Debug, Serialize)]
pub struct CaseResult {
    pub elapsed_ms: u64,
    pub name: String,
    pub passed: bool,
    pub feedback: String,
    pub run: RunView,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub suite_digest: String,
    pub agent: VersionRef,
    pub passed: bool,
    pub cases: Vec<CaseResult>,
}

pub fn validate_cases(cases: &[Case]) -> Result<()> {
    if cases.is_empty() || cases.len() > 100 {
        return Err(Error::Invalid("evaluation requires 1 to 100 cases".into()));
    }
    let mut names = BTreeSet::new();
    for case in cases {
        if case.name.trim().is_empty() || case.name.len() > 256 || !names.insert(&case.name) {
            return Err(Error::Invalid(
                "evaluation case names must be nonempty and unique".into(),
            ));
        }
        definitions::validate_schema(&case.expected_schema)?;
        for criterion in &case.criteria {
            criterion.validate()?;
        }
    }
    Ok(())
}

/// Every case is a fresh Run. Approval/uncertainty waits count as incomplete,
/// never auto-approved or retried. Tools execute normally: use test destinations.
/// Definitions, configured goals, output verification, and limits stay unchanged.
pub fn evaluate<B: Backend, M: ModelExecutor, T: ToolExecutor>(
    runtime: &mut Runtime<B, M, T>,
    actor: &Actor,
    agent: VersionRef,
    goal: Option<Goal>,
    cases: &[Case],
) -> Result<Report> {
    validate_cases(cases)?;
    let mut results = Vec::with_capacity(cases.len());
    for case in cases {
        let started = std::time::Instant::now();
        let id = runtime.submit_with_goal(
            actor,
            agent.clone(),
            case.input.clone(),
            None,
            goal.clone(),
        )?;
        let mut run = runtime.store.inspect(actor, id)?;
        let mut error = None;
        // Host safeguard in addition to each agent's model/tool/step budgets.
        for _ in 0..10_000 {
            match runtime.tick(actor, id) {
                Ok(view) => run = view,
                Err(failure) => {
                    error = Some(failure.to_string());
                    break;
                }
            }
            if run.status.terminal()
                || matches!(run.status, RunStatus::Waiting | RunStatus::Cancelling)
            {
                break;
            }
            if runtime.store.operations(actor, id)?.iter().any(|op| {
                matches!(
                    op.status,
                    OperationStatus::Running | OperationStatus::Unknown
                )
            }) {
                break;
            }
        }
        let source_operations = runtime.store.operations(actor, id)?;
        let assessment = if let Some(error) = error {
            Err(error)
        } else {
            assess_completed(case, &run, &source_operations)
        };
        results.push(CaseResult {
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            name: case.name.clone(),
            passed: assessment.is_ok(),
            feedback: assessment
                .err()
                .unwrap_or_else(|| "expected output contract passed".into()),
            run,
        });
    }
    Ok(Report {
        suite_digest: definitions::digest(&cases)?,
        agent,
        passed: results.iter().all(|case| case.passed),
        cases: results,
    })
}

pub(crate) fn assess_completed(
    case: &Case,
    run: &RunView,
    operations: &[Operation],
) -> std::result::Result<(), String> {
    if run.status != RunStatus::Completed {
        return Err(format!("run did not complete: {:?}", run.status));
    }
    if !run
        .assessment
        .as_ref()
        .is_some_and(|assessment| assessment.passed)
    {
        return Err("runtime verification did not pass".into());
    }
    // Some(Value::Null) is a valid final result; None is not.
    let value = run.result.as_ref().ok_or("completed run has no result")?;
    definitions::validate(&case.expected_schema, value).map_err(|e| e.to_string())?;
    if let Some(index) = case
        .criteria
        .iter()
        .position(|rule| !rule.matches_with_operations(value, operations))
    {
        return Err(format!("held-out criterion {} failed", index + 1));
    }
    Ok(())
}
