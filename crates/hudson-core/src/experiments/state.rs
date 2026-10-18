use super::*;
use crate::{
    configured::Configuration,
    memory::*,
    storage::{memory::Data, Store},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub protocol: u32,
    pub actor: Actor,
    pub spec: Spec,
    pub configuration: Value,
    pub request_digest: String,
    pub manifest_digest: String,
    pub group: String,
    pub name: String,
    pub lesson_scope: MemoryScope,
    pub snapshot: Vec<MemoryRecord>,
    pub status: Status,
    pub cursor: usize,
    pub runs: BTreeMap<usize, Uuid>,
    pub verdicts: BTreeMap<usize, bool>,
    pub selected_version: u32,
    pub lesson_id: Option<Uuid>,
    pub reason: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) struct RunScope {
    pub experiment: Uuid,
    pub index: usize,
}

pub(crate) fn owned<'a>(d: &'a Data, actor: &Actor, id: Uuid) -> Result<&'a Record> {
    let record = d
        .experiments
        .get(&id)
        .filter(|record| record.actor == *actor)
        .ok_or(Error::NotFound)?;
    if record.protocol != 1 {
        return Err(Error::Unsupported("unknown experiment protocol".into()));
    }
    let budget = d
        .model_budgets
        .get(&(actor.workspace_id.clone(), record.group.clone()));
    if (!record.runs.is_empty() && budget.is_none())
        || budget.is_some_and(|budget| {
            budget.limit != record.spec.max_model_calls || budget.admitted > budget.limit
        })
    {
        return Err(Error::Conflict(
            "saved experiment admission counter is missing or inconsistent".into(),
        ));
    }
    let stages = 8 + 2 * record.spec.validation.len();
    if record.status == Status::Waiting
        || !(1..=8).contains(&record.spec.validation.len())
        || record.cursor > stages
        || record.runs.keys().any(|index| *index >= stages)
        || (0..record.cursor).any(|index| !record.runs.contains_key(&index))
        || record.runs.keys().any(|index| *index > record.cursor)
        || record
            .runs
            .values()
            .any(|run| d.experiment_runs.get(run) != Some(&id) || d.run(actor, *run).is_err())
    {
        return Err(Error::Conflict(
            "saved experiment progress exceeds its stage bounds".into(),
        ));
    }
    Ok(record)
}
pub(crate) fn check_scope(d: &Data, actor: &Actor, scope: &RunScope) -> Result<()> {
    let record = owned(d, actor, scope.experiment)?;
    if record.status != Status::Running || record.cursor != scope.index {
        return Err(Error::Conflict(
            "experiment stage is no longer active".into(),
        ));
    }
    Ok(())
}
pub(crate) fn attach_run(d: &mut Data, scope: &RunScope, id: Uuid) -> Result<()> {
    let record = d
        .experiments
        .get_mut(&scope.experiment)
        .expect("checked experiment");
    if record
        .runs
        .get(&scope.index)
        .is_some_and(|existing| *existing != id)
    {
        return Err(Error::Conflict("experiment stage already has a run".into()));
    }
    record.runs.insert(scope.index, id);
    d.experiment_runs.insert(id, scope.experiment);
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum Stage {
    Probe(u32),
    Edit(&'static str, u32),
    Validation(u32, usize),
}
impl Stage {
    pub fn label(self) -> String {
        match self {
            Self::Probe(version) => format!("probe-{version}"),
            Self::Edit(role, _) => role.into(),
            Self::Validation(version, index) => format!("validation-{version}-{index}"),
        }
    }
    pub fn version(self) -> u32 {
        match self {
            Self::Probe(v) | Self::Edit(_, v) | Self::Validation(v, _) => v,
        }
    }
}
pub(crate) fn stages(count: usize) -> Vec<Stage> {
    let mut stages = vec![
        Stage::Probe(1),
        Stage::Edit("general", 5),
        Stage::Probe(2),
        Stage::Edit("specific", 6),
        Stage::Probe(3),
        Stage::Edit("compose", 7),
        Stage::Probe(4),
        Stage::Edit("reflect", 8),
    ];
    stages.extend((0..count).map(|i| Stage::Validation(1, i)));
    stages.extend((0..count).map(|i| Stage::Validation(4, i)));
    stages
}

impl Store {
    pub fn inspect_experiment(&self, actor: &Actor, id: Uuid) -> Result<View> {
        self.read(|d| {
            let record = owned(d, actor, id)?;
            let plan = record.spec.plan()?;
            let stages = stages(record.spec.validation.len());
            let mut views = Vec::with_capacity(record.runs.len());
            let mut waiting = false;
            for (index, run_id) in &record.runs {
                let run = d.run(actor, *run_id)?;
                if *index == record.cursor
                    && (matches!(run.status, RunStatus::Waiting | RunStatus::Cancelling)
                        || run.pending_operations.iter().any(|id| {
                            matches!(
                                d.operations[id].status,
                                OperationStatus::Unknown | OperationStatus::Running
                            )
                        }))
                {
                    waiting = true;
                }
                views.push(StageView {
                    stage: stages[*index].label(),
                    run: RunView::from(run),
                });
            }
            let status = if waiting && record.status == Status::Running {
                Status::Waiting
            } else {
                record.status
            };
            let mut comparisons = Vec::with_capacity(record.spec.validation.len());
            for (index, case) in record.spec.validation.iter().enumerate() {
                let base = record.runs.get(&(8 + index));
                let candidate = record.runs.get(&(8 + record.spec.validation.len() + index));
                if let (Some(base), Some(candidate)) = (base, candidate) {
                    comparisons.push(Comparison {
                        case: case.name.clone(),
                        baseline_run: *base,
                        candidate_run: *candidate,
                        baseline_passed: record.verdicts.get(&(8 + index)).copied(),
                        candidate_passed: record
                            .verdicts
                            .get(&(8 + record.spec.validation.len() + index))
                            .copied(),
                    });
                }
            }
            let candidate = if record.status == Status::Completed && record.selected_version == 4 {
                let run = d.run(actor, record.runs[&5])?;
                Some(serde_json::from_value(run.result.clone().ok_or_else(
                    || Error::Conflict("composition result missing".into()),
                )?)?)
            } else {
                None
            };
            Ok(View {
                id,
                status,
                manifest_digest: record.manifest_digest.clone(),
                plan,
                admitted_model_calls: d
                    .model_budgets
                    .get(&(actor.workspace_id.clone(), record.group.clone()))
                    .map_or(0, |budget| budget.admitted),
                selected: VersionRef {
                    id: record.name.clone(),
                    version: record.selected_version,
                },
                candidate,
                comparisons,
                stages: views,
                lesson_id: record.lesson_id,
                lesson_snapshot: record.snapshot.iter().map(|r| r.id).collect(),
                reason: record.reason.clone(),
            })
        })
    }

    /// Atomically stop new stage admissions and fence every admitted stage run.
    pub fn cancel_experiment(&self, actor: &Actor, id: Uuid) -> Result<()> {
        self.transact(|d| {
            let record = owned(d, actor, id)?;
            if record.status.terminal() {
                return Ok(());
            }
            let runs = record.runs.values().copied().collect::<Vec<_>>();
            d.experiments.get_mut(&id).expect("checked").status = Status::Cancelling;
            for run_id in runs {
                let run = d.runs.get_mut(&run_id).expect("attached run");
                if run.status.terminal() || run.status == RunStatus::Cancelling {
                    continue;
                }
                run.status = RunStatus::Cancelling;
                run.next_input = None;
                run.revision += 1;
                crate::inbox::discard(d, run_id);
                d.event(
                    run_id,
                    None,
                    EventType::CancellationRequested,
                    Some(actor.id.clone()),
                    json!({"experiment_id":id}),
                );
            }
            Ok(())
        })
    }
}

pub(crate) fn restore(record: &Record) -> Result<Configuration> {
    Configuration::restore_publication(record.configuration.clone())
        .map_err(|_| Error::Conflict("saved experiment configuration is invalid".into()))
}
