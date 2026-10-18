use super::{state::*, *};
use crate::{
    configured::{AgentRuntime, Configuration},
    memory::*,
    runtime::SubmissionScope,
    storage::Store,
};

pub struct Driver {
    store: Store,
    actor: Actor,
    id: Uuid,
    record: Option<Record>,
    configuration: Option<Configuration>,
    runtimes: BTreeMap<u32, (AgentRuntime, VersionRef, Option<Goal>)>,
}

impl Driver {
    pub fn plan(configuration: &Configuration, spec: &Spec) -> Result<Plan> {
        let plan = spec.plan()?;
        configuration.experiment_template()?;
        configuration.experiment_input(&spec.probe)?;
        for case in &spec.validation {
            configuration.experiment_input(&case.input)?;
        }
        crate::runtime::bounded(&configuration.freeze_publication()?, 128 * 1024)?;
        Ok(plan)
    }

    /// Resume the exact stored manifest; no caller-supplied configuration is substituted.
    pub fn resume(store: Store, actor: Actor, id: Uuid) -> Result<Self> {
        let record = store.read(|d| Ok(owned(d, &actor, id)?.clone()))?;
        let request = definitions::digest(&(1, &actor, &record.spec, &record.configuration))?;
        if request != record.request_digest
            || definitions::digest(&(&request, &record.snapshot))? != record.manifest_digest
        {
            return Err(Error::Conflict(
                "saved experiment manifest does not match its fingerprints".into(),
            ));
        }
        Ok(Self {
            store,
            actor,
            id,
            record: Some(record),
            configuration: None,
            runtimes: BTreeMap::new(),
        })
    }

    pub fn open(
        store: Store,
        actor: Actor,
        configuration: Configuration,
        spec: Spec,
    ) -> Result<Self> {
        Self::plan(&configuration, &spec)?;
        if actor.id.trim().is_empty()
            || actor.workspace_id.trim().is_empty()
            || actor.id.len() > 128
            || actor.workspace_id.len() > 128
        {
            return Err(Error::Denied);
        }
        let frozen = configuration.freeze_publication()?;
        crate::runtime::bounded(&frozen, 128 * 1024)?;
        let request_digest = definitions::digest(&(1, &actor, &spec, &frozen))?;
        let key = definitions::digest(&("offline-experiment-v1", &actor, &spec.request_key))?;
        let id = Uuid::parse_str(&key[..32]).expect("digest is valid UUID hex");
        let existing = store.read(|d| {
            if let Some(record) = d.experiments.get(&id) {
                if record.actor != actor || record.request_digest != request_digest {
                    return Err(Error::Conflict(
                        "experiment key reused with changed configuration, task, suite or budget"
                            .into(),
                    ));
                }
                return Ok(true);
            }
            Ok(false)
        })?;
        if !existing {
            let scope = MemoryScope {
                name: format!(
                    "experiment-{}",
                    definitions::digest(&(&spec.domain, &frozen))?
                ),
            };
            store.transact(|d| {
                if let Some(record) = d.experiments.get(&id) {
                    if record.actor != actor || record.request_digest != request_digest {
                        return Err(Error::Conflict("experiment key changed".into()));
                    }
                    return Ok(());
                }
                if d.model_budgets
                    .contains_key(&(actor.workspace_id.clone(), format!("experiment-{key}")))
                {
                    return Err(Error::Conflict(
                        "experiment budget exists without its manifest; inspect before continuing"
                            .into(),
                    ));
                }
                // Keep at most four prior accepted lessons. They remain untrusted claims.
                let mut prior = Vec::with_capacity(5);
                for memory in d
                    .experiments
                    .values()
                    .filter(|previous| {
                        previous.actor == actor
                            && previous.lesson_scope == scope
                            && previous.status == Status::Completed
                            && previous.selected_version == 4
                    })
                    .filter_map(|previous| previous.lesson_id)
                    .filter_map(|id| d.memories.get(&id))
                    .filter(|memory| {
                        memory.workspace_id == actor.workspace_id
                            && memory.actor_id == actor.id
                            && memory.scope == scope.name
                            && !memory.deleted
                            && memory.superseded_by.is_none()
                            && memory.content.text.len() <= 4096
                    })
                {
                    prior.push(memory);
                    prior.sort_by_key(|memory| (std::cmp::Reverse(memory.created_at), memory.id));
                    prior.truncate(4);
                }
                let snapshot = prior.into_iter().cloned().collect::<Vec<_>>();
                let manifest_digest = definitions::digest(&(&request_digest, &snapshot))?;
                d.experiments.insert(
                    id,
                    Record {
                        protocol: 1,
                        actor: actor.clone(),
                        spec: spec.clone(),
                        configuration: frozen,
                        request_digest,
                        manifest_digest,
                        group: format!("experiment-{key}"),
                        name: format!("experiment-{id}"),
                        lesson_scope: scope,
                        snapshot,
                        status: Status::Running,
                        cursor: 0,
                        runs: BTreeMap::new(),
                        verdicts: BTreeMap::new(),
                        selected_version: 1,
                        lesson_id: None,
                        reason: None,
                    },
                );
                Ok(())
            })?;
        }
        let mut driver = Self::resume(store, actor, id)?;
        driver.configuration = Some(configuration);
        Ok(driver)
    }

    pub fn id(&self) -> Uuid {
        self.id
    }
    pub fn inspect(&self) -> Result<View> {
        self.store.inspect_experiment(&self.actor, self.id)
    }
    pub fn cancel(&self) -> Result<()> {
        self.store.cancel_experiment(&self.actor, self.id)
    }

    /// Release idle HTTP clients and parsed configuration without changing durable
    /// progress, receipts, frozen inputs or the experiment's admission counter.
    pub fn release_resources(&mut self) {
        self.runtimes.clear();
        self.configuration = None;
    }

    /// At most one ordinary runtime tick. Existing running/unknown effects are never replayed.
    pub fn tick(&mut self) -> Result<View> {
        let mut record = self
            .record
            .take()
            .expect("driver manifest restored after every tick");
        let result = (|| {
            self.store.read(|d| {
                let current = owned(d, &self.actor, self.id)?;
                record.status = current.status;
                record.cursor = current.cursor;
                record.runs.clone_from(&current.runs);
                record.verdicts.clone_from(&current.verdicts);
                record.selected_version = current.selected_version;
                record.lesson_id = current.lesson_id;
                record.reason.clone_from(&current.reason);
                Ok(())
            })?;
            self.tick_record(&record)
        })();
        self.record = Some(record);
        result
    }

    fn tick_record(&mut self, record: &Record) -> Result<View> {
        if record.status.terminal() {
            return self.inspect();
        }
        let plan = stages(record.spec.validation.len());
        if record.status == Status::Cancelling {
            let runtime = crate::runtime::Runtime::new(
                self.store.clone(),
                hudson_harness::AgentLoop,
                CancelOnly,
                crate::adapters::tools::ToolRegistry::new(),
            );
            for id in record.runs.values() {
                runtime.cancel(&self.actor, *id)?;
            }
            self.store.transact(|d| {
                if owned(d, &self.actor, self.id)?
                    .runs
                    .values()
                    .all(|id| d.runs[id].status.terminal())
                {
                    d.experiments.get_mut(&self.id).expect("checked").status = Status::Cancelled;
                }
                Ok(())
            })?;
            return self.inspect();
        }
        if record.cursor == plan.len() {
            return self.finish(record);
        }
        let stage = plan[record.cursor];
        let prepared = self.prepare(record, stage);
        let input = match prepared {
            Ok(value) => value,
            Err(Error::Conflict(message)) => return Err(Error::Conflict(message)),
            Err(_) => {
                self.fail(
                    record,
                    "experiment stage could not be prepared within its fixed controls",
                )?;
                return self.inspect();
            }
        };
        let (runtime, reference, goal) = self
            .runtimes
            .get_mut(&stage.version())
            .expect("prepared runtime");
        let run_id = if let Some(id) = record.runs.get(&record.cursor) {
            self.store
                .validate_resume(&self.actor, *id, reference, goal)?;
            *id
        } else {
            runtime.submit_scoped(
                &self.actor,
                reference.clone(),
                input.expect("new stage input"),
                Some(format!("experiment:{}:{}", self.id, record.cursor)),
                goal.clone(),
                SubmissionScope::Experiment(RunScope {
                    experiment: self.id,
                    index: record.cursor,
                }),
            )?
        };
        let view = runtime.tick(&self.actor, run_id)?;
        if view.status == RunStatus::Completed {
            let valid = match stage {
                Stage::Edit("reflect", _) => self.lesson(record, &view).map(|_| ()),
                Stage::Edit(_, _) => {
                    serde_json::from_value::<Variant>(view.result.clone().ok_or(Error::NotFound)?)
                        .map_err(Error::from)
                        .and_then(|variant| {
                            variant.validate(
                                &self
                                    .configuration
                                    .as_ref()
                                    .expect("prepared template")
                                    .experiment_template()?,
                            )
                        })
                }
                _ => Ok(()),
            };
            if valid.is_err() {
                self.fail(
                    record,
                    "experiment stage returned an invalid bounded variant or lesson",
                )?;
            } else {
                let verdict = if let Stage::Validation(_, index) = stage {
                    Some(
                        evaluation::assess_completed(
                            &record.spec.validation[index],
                            &view,
                            &self.store.operations(&self.actor, run_id)?,
                        )
                        .is_ok(),
                    )
                } else {
                    None
                };
                self.store.transact(|d| {
                    let current = d.experiments.get_mut(&self.id).ok_or(Error::NotFound)?;
                    if current.status == Status::Running
                        && current.cursor == record.cursor
                        && current.runs.get(&record.cursor) == Some(&run_id)
                    {
                        if let Some(verdict) = verdict {
                            current.verdicts.insert(record.cursor, verdict);
                        }
                        current.cursor += 1;
                    }
                    Ok(())
                })?;
            }
        } else if view.status == RunStatus::Failed {
            self.fail(
                record,
                "experiment trial failed; no quality score or promotion was recorded",
            )?;
        } else if matches!(view.status, RunStatus::Cancelled | RunStatus::Cancelling) {
            self.cancel()?;
        }
        self.inspect()
    }

    fn runtime(
        &self,
        record: &Record,
        configuration: &Configuration,
        stage: Stage,
    ) -> Result<(AgentRuntime, VersionRef, Option<Goal>)> {
        let baseline = configuration.experiment_template()?;
        let version = stage.version();
        let variant = match version {
            2 => self.variant(record, 1)?,
            3 => self.variant(record, 3)?,
            4 => self.variant(record, 5)?,
            _ => baseline,
        };
        let role = match stage {
            Stage::Edit(role, _) => Some(role),
            _ => None,
        };
        configuration
            .experiment_configuration(
                &record.name,
                version,
                &variant,
                &record.group,
                record.spec.max_model_calls,
                role,
            )?
            .build_fixture_experiment(self.store.clone(), &self.actor)
            .map_err(|error| Error::Conflict(error.to_string()))
    }

    fn prepare(&mut self, record: &Record, stage: Stage) -> Result<Option<Value>> {
        if self.configuration.is_none() {
            self.configuration = Some(restore(record)?);
        }
        let configuration = self.configuration.as_ref().expect("restored template");
        let runtime = if self.runtimes.contains_key(&stage.version()) {
            None
        } else {
            Some(self.runtime(record, configuration, stage)?)
        };
        let input = if record.runs.contains_key(&record.cursor) {
            None
        } else {
            Some(self.input(record, configuration, stage)?)
        };
        if let Some(runtime) = runtime {
            self.runtimes.insert(stage.version(), runtime);
        }
        Ok(input)
    }

    fn variant(&self, record: &Record, index: usize) -> Result<Variant> {
        let run = self.store.inspect(&self.actor, record.runs[&index])?;
        Ok(serde_json::from_value(run.result.ok_or(Error::NotFound)?)?)
    }

    fn input(&self, record: &Record, configuration: &Configuration, stage: Stage) -> Result<Value> {
        match stage {
            Stage::Probe(_) => Ok(record.spec.probe.clone()),
            Stage::Validation(_, index) => Ok(record.spec.validation[index].input.clone()),
            Stage::Edit(role, _) => {
                let baseline = configuration.experiment_template()?;
                let indices: &[usize] = if matches!(role, "compose" | "reflect") {
                    &[0, 2, 4]
                } else {
                    &[0]
                };
                let mut evidence = Vec::with_capacity(4);
                for index in indices
                    .iter()
                    .copied()
                    .chain((role == "reflect").then_some(6))
                {
                    let run = self.store.inspect(&self.actor, record.runs[&index])?;
                    let tools = self.store.read(|data| {
                        let mut operations = data.operations.values().filter(|op| op.run_id == run.id && matches!(op.request, OperationRequest::Tool { .. })).take(17).collect::<Vec<_>>();
                        if operations.len() > 16 { return Err(Error::Invalid("probe tool evidence exceeds sixteen operations".into())); }
                        operations.sort_by_key(|op| (op.step_index, op.request_index));
                        operations.into_iter().map(|operation| {
                            let OperationRequest::Tool { call, .. } = &operation.request else { unreachable!() };
                            Ok(json!({"operation_id":operation.meta.id,"tool":call.name,"status":operation.status,
                                "request_digest":operation.request_digest,"result_digest":operation.result.as_ref().map(definitions::digest).transpose()?}))
                        }).collect::<Result<Vec<_>>>()
                    })?;
                    evidence.push(json!({"run_id":run.id,"variant_version":run.agent_ref.version,"status":run.status,
                        "output":run.result,"usage":run.usage,"tool_summaries":tools}));
                }
                let mut variants = if matches!(role, "compose" | "reflect") {
                    vec![
                        json!({"version":2,"variant":self.variant(record, 1)?}),
                        json!({"version":3,"variant":self.variant(record, 3)?}),
                    ]
                } else {
                    vec![]
                };
                if role == "reflect" {
                    variants.push(json!({"version":4,"variant":self.variant(record, 5)?}));
                }
                let input = json!({"stage":role,"baseline_instructions":baseline.instructions,"skills":baseline.skills,
                    "fixed_contract":configuration.experiment_contract(),"domain":record.spec.domain,"task":record.spec.probe,"probe_executions":evidence,"proposals":variants,"prior_lessons":record.snapshot});
                crate::runtime::bounded(&input, 65536)?;
                Ok(input)
            }
        }
    }

    fn lesson(&self, record: &Record, view: &RunView) -> Result<Lesson> {
        let lesson: Lesson = serde_json::from_value(view.result.clone().ok_or(Error::NotFound)?)?;
        lesson.validate(&[
            record.runs[&0],
            record.runs[&2],
            record.runs[&4],
            record.runs[&6],
        ])?;
        Ok(lesson)
    }

    fn fail(&self, record: &Record, reason: &str) -> Result<()> {
        self.store.transact(|d| {
            let current = d.experiments.get_mut(&self.id).ok_or(Error::NotFound)?;
            if current.status == Status::Running && current.cursor == record.cursor {
                current.status = Status::Failed;
                current.reason = Some(reason.into());
            }
            Ok(())
        })
    }

    fn finish(&self, record: &Record) -> Result<View> {
        let view = self.inspect()?;
        let all_scored = view.comparisons.len() == record.spec.validation.len()
            && view
                .comparisons
                .iter()
                .all(|case| case.baseline_passed.is_some() && case.candidate_passed.is_some());
        if !all_scored {
            self.fail(
                record,
                "held-out execution or assessment is incomplete; baseline retained",
            )?;
            return self.inspect();
        }
        let improves = view
            .comparisons
            .iter()
            .any(|case| case.baseline_passed == Some(false) && case.candidate_passed == Some(true));
        let regresses = view
            .comparisons
            .iter()
            .any(|case| case.baseline_passed == Some(true) && case.candidate_passed != Some(true));
        let promote = all_scored && improves && !regresses;
        let reflection = self.store.inspect(&self.actor, record.runs[&7])?;
        let lesson = self.lesson(record, &reflection)?;
        self.store.transact(|d| {
            let current = owned(d, &self.actor, self.id)?;
            if current.status != Status::Running || current.cursor != record.cursor {
                return Ok(());
            }
            let lesson_id = if promote {
                // Selection, lesson retention and completion commit together. This is
                // an inference supported by visible executions, never grader data.
                for id in &lesson.run_ids {
                    d.run(&self.actor, *id)?;
                }
                let id = Uuid::new_v4();
                let timestamp = now();
                let memory = MemoryRecord {
                    id,
                    workspace_id: self.actor.workspace_id.clone(),
                    actor_id: self.actor.id.clone(),
                    scope: record.lesson_scope.name.clone(),
                    created_at: timestamp,
                    updated_at: timestamp,
                    deleted: false,
                    superseded_by: None,
                    request_key: format!("experiment:{}:lesson", self.id),
                    content: RetainMemory {
                        text: format!("{}\n{}", lesson.cue, lesson.lesson),
                        kind: MemoryKind::Inference,
                        sources: lesson
                            .run_ids
                            .iter()
                            .map(|run_id| MemorySource {
                                reference: format!(
                                    "experiment:{}:{}",
                                    self.id, record.manifest_digest
                                ),
                                run_id: Some(*run_id),
                                operation_id: None,
                            })
                            .collect(),
                        supersedes: None,
                    },
                };
                d.memories.insert(id, memory);
                Some(id)
            } else {
                None
            };
            let current = d.experiments.get_mut(&self.id).expect("checked");
            current.status = Status::Completed;
            current.selected_version = if promote { 4 } else { 1 };
            current.lesson_id = lesson_id;
            current.reason = Some(
                if promote {
                    "candidate improved held-out results without a baseline regression"
                } else {
                    "baseline retained: no strict held-out improvement without regressions"
                }
                .into(),
            );
            Ok(())
        })?;
        self.inspect()
    }
}

// Cancellation only reads/settles durable records and requires no fixture endpoint.
struct CancelOnly;
impl crate::adapters::models::ModelExecutor for CancelOnly {
    fn call(
        &mut self,
        _: &hudson_harness::ModelRequest,
    ) -> std::result::Result<hudson_harness::ModelResponse, crate::adapters::tools::ExecutionError>
    {
        Err(crate::adapters::tools::ExecutionError::Failed(
            "cancellation cannot call a model".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_unscoped_submission_is_rejected_before_dispatch() {
        let store = Store::default();
        let actor = Actor {
            workspace_id: "scope-unit".into(),
            id: "operator".into(),
        };
        let configuration = Configuration::from_effective_definition(json!({
            "name":"fixture","instructions":"Return a label","model":"fixture",
            "endpoint":"http://127.0.0.1:1/model"
        }))
        .unwrap();
        let spec: Spec = serde_json::from_value(json!({
            "request_key":"scope-collision","domain":"labels","probe":"visible","max_model_calls":8,
            "validation":[{"name":"held-out","input":"other","expected_schema":{"type":"string"}}]
        }))
        .unwrap();
        let mut driver = Driver::open(store.clone(), actor.clone(), configuration, spec).unwrap();
        // One tick records a model intent but performs no provider request.
        let first = driver.tick().unwrap();
        let run = first.stages[0].run.id;
        store
            .transact(|data| {
                data.experiments
                    .get_mut(&driver.id())
                    .unwrap()
                    .runs
                    .remove(&0);
                data.experiment_runs.remove(&run);
                Ok(())
            })
            .unwrap();
        assert!(
            matches!(driver.tick(), Err(Error::Conflict(message)) if message.contains("scope"))
        );
        assert!(driver.inspect().unwrap().stages.is_empty());
        assert_eq!(driver.inspect().unwrap().admitted_model_calls, 0);
        assert_eq!(
            store.operations(&actor, run).unwrap()[0].status,
            OperationStatus::Pending
        );
    }
}
