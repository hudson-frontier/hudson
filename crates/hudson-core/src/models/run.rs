use super::{Limits, Metadata, Usage, VersionRef};
use hudson_harness::{Checkpoint, Input};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Waiting,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WaitReason {
    Approval {
        operation_id: Uuid,
    },
    Reconciliation {
        operation_id: Uuid,
    },
    UserInput {
        prompt: String,
        #[serde(default)]
        question_id: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub operation_id: Uuid,
    pub tool_name: String,
    pub request_digest: String,
    pub result_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Assessment {
    pub passed: bool,
    pub feedback: String,
    /// Successful recorded tool executions; provenance does not establish semantic truth.
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

/// Immutable task objective and deterministic final-output acceptance contract.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Goal {
    pub objective: String,
    pub success_schema: Value,
    #[serde(default)]
    pub criteria: Vec<crate::verification::Criterion>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Run {
    pub meta: Metadata,
    pub agent_ref: VersionRef,
    #[serde(default)]
    pub parent_operation: Option<Uuid>,
    #[serde(default)]
    pub model_budget: Option<crate::budgets::ModelBudgetBinding>,
    pub actor_id: String,
    pub request_key: Option<String>,
    pub input_digest: String,
    pub input: Value,
    #[serde(default)]
    pub goal: Option<Goal>,
    pub status: RunStatus,
    pub reason: Option<String>,
    pub wait: Option<WaitReason>,
    pub state: Checkpoint,
    pub pending_operations: Vec<Uuid>,
    pub next_input: Option<Input>,
    pub limits: Limits,
    pub usage: Usage,
    pub result: Option<Value>,
    pub assessment: Option<Assessment>,
    pub verified_candidate_digest: Option<String>,
    pub revision: u64,
}

/// Deliberately excludes checkpoint, credentials, raw requests, and worker internals.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunView {
    pub agent_ref: VersionRef,
    pub parent_operation: Option<Uuid>,
    pub goal: Option<Goal>,
    pub id: Uuid,
    pub status: RunStatus,
    pub wait: Option<WaitReason>,
    pub reason: Option<String>,
    pub result: Option<Value>,
    pub usage: Usage,
    pub assessment: Option<Assessment>,
}
impl Run {
    fn has_verified_null_result(&self) -> bool {
        self.status == RunStatus::Completed
            && self
                .assessment
                .as_ref()
                .is_some_and(|assessment| assessment.passed)
            && self
                .verified_candidate_digest
                .as_deref()
                .is_some_and(|expected| {
                    crate::definitions::digest(&Value::Null).is_ok_and(|actual| actual == expected)
                })
    }

    pub(crate) fn restore_verified_null_result(&mut self) {
        if self.result.is_none() && self.has_verified_null_result() {
            self.result = Some(Value::Null);
        }
    }
}
impl From<&Run> for RunView {
    fn from(run: &Run) -> Self {
        // JSONB encodes Some(null) and None identically. A completed run's exact
        // verified-output receipt disambiguates null without guessing a result.
        let result = run
            .result
            .clone()
            .or_else(|| run.has_verified_null_result().then_some(Value::Null));
        Self {
            agent_ref: run.agent_ref.clone(),
            id: run.meta.id,
            parent_operation: run.parent_operation,
            goal: run.goal.clone(),
            status: run.status,
            wait: run.wait.clone(),
            reason: run.reason.clone(),
            result,
            usage: run.usage.clone(),
            assessment: run.assessment.clone(),
        }
    }
}
