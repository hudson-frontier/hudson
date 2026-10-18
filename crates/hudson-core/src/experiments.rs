//! One bounded offline refinement step. All effects use ordinary durable runs.
mod driver;
mod state;

use crate::{definitions, evaluation, models::*, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

pub use driver::Driver;
pub(crate) use state::{attach_run, check_scope, Record, RunScope};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub request_key: String,
    pub domain: String,
    pub probe: Value,
    pub validation: Vec<evaluation::Case>,
    pub max_model_calls: u32,
}
impl Spec {
    pub fn validate(&self) -> Result<()> {
        if self.request_key.is_empty()
            || self.request_key.len() > 128
            || self.request_key.chars().any(char::is_control)
            || self.domain.trim().is_empty()
            || self.domain.len() > 128
            || !(1..=256).contains(&self.max_model_calls)
            || self.validation.len() > 8
            || serde_json::to_vec(self)?.len() > 128 * 1024
            || serde_json::to_vec(&self.probe)?.len() > 4096
        {
            return Err(Error::Invalid(
                "experiment keys/domain, payloads, validation cases or model budget exceed bounds"
                    .into(),
            ));
        }
        evaluation::validate_cases(&self.validation)?;
        let probe = definitions::digest(&self.probe)?;
        let mut inputs = std::collections::BTreeSet::new();
        for case in &self.validation {
            let digest = definitions::digest(&case.input)?;
            if digest == probe || !inputs.insert(digest) {
                return Err(Error::Invalid(
                    "probe and held-out inputs must be distinct and unique".into(),
                ));
            }
        }
        Ok(())
    }
    /// No store, model, tool, or credential access.
    pub fn plan(&self) -> Result<Plan> {
        self.validate()?;
        Ok(Plan {
            refinement_steps: 1,
            probe_runs: 4,
            editing_runs: 4,
            validation_runs: 2 * self.validation.len(),
            max_model_calls: self.max_model_calls,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub instructions: String,
    #[serde(default)]
    pub skills: BTreeMap<String, String>,
}
impl Variant {
    pub(crate) fn validate(&self, baseline: &Self) -> Result<()> {
        if self.instructions.trim().is_empty()
            || self.instructions.len() > 16384
            || self.skills.len() > 8
            || serde_json::to_vec(self)?.len() > 32768
            || self.skills.iter().any(|(name, text)| {
                !baseline.skills.contains_key(name) || text.trim().is_empty() || text.len() > 4096
            })
        {
            return Err(Error::Invalid(
                "variant may edit only bounded instructions and existing skill bodies".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lesson {
    pub cue: String,
    pub lesson: String,
    pub run_ids: Vec<Uuid>,
}
impl Lesson {
    pub(crate) fn schema() -> Value {
        json!({"type":"object","additionalProperties":false,"required":["cue","lesson","run_ids"],"properties":{
            "cue":{"type":"string","minLength":1,"maxLength":256},
            "lesson":{"type":"string","minLength":1,"maxLength":2048},
            "run_ids":{"type":"array","minItems":1,"maxItems":4,"uniqueItems":true,"items":{"type":"string","format":"uuid"}}
        }})
    }
    pub(crate) fn validate(&self, visible_runs: &[Uuid]) -> Result<()> {
        let unique: std::collections::BTreeSet<_> = self.run_ids.iter().collect();
        if self.cue.trim().is_empty()
            || self.cue.len() > 256
            || self.lesson.trim().is_empty()
            || self.lesson.len() > 2048
            || self.run_ids.is_empty()
            || self.run_ids.len() > 4
            || unique.len() != self.run_ids.len()
            || self.run_ids.iter().any(|id| !visible_runs.contains(id))
        {
            return Err(Error::Invalid(
                "lesson requires bounded text and references to its visible probe runs".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub refinement_steps: u8,
    pub probe_runs: usize,
    pub editing_runs: usize,
    pub validation_runs: usize,
    pub max_model_calls: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Waiting,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}
impl Status {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comparison {
    pub case: String,
    pub baseline_run: Uuid,
    pub candidate_run: Uuid,
    /// None means an incomplete execution, not a quality score of zero.
    pub baseline_passed: Option<bool>,
    pub candidate_passed: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageView {
    pub stage: String,
    pub run: RunView,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct View {
    pub id: Uuid,
    pub status: Status,
    pub manifest_digest: String,
    pub plan: Plan,
    pub admitted_model_calls: u32,
    pub selected: VersionRef,
    pub candidate: Option<Variant>,
    pub comparisons: Vec<Comparison>,
    pub stages: Vec<StageView>,
    pub lesson_id: Option<Uuid>,
    pub lesson_snapshot: Vec<Uuid>,
    pub reason: Option<String>,
}
