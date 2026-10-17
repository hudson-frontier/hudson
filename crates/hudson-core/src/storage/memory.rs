use crate::{models::*, Error, Result};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Data {
    #[serde(default)]
    pub sandbox_bindings: BTreeMap<String, crate::sandbox_binding::Binding>,
    #[serde(default, with = "super::pairs")]
    pub customer_registry:
        BTreeMap<(String, String, String, VersionRef), crate::customer::RegistryEntry>,
    #[serde(default, with = "super::pairs")]
    pub customer_tool_keys: BTreeMap<(String, String, String), crate::customer::DefinitionReceipt>,
    #[serde(default, with = "super::pairs")]
    pub publications: BTreeMap<(String, VersionRef), crate::publication::PublishedConfiguration>,
    #[serde(default, with = "super::pairs")]
    pub publication_keys:
        BTreeMap<(String, String, String), crate::publication::PublicationReceipt>,
    #[serde(default)]
    pub api_tokens: BTreeMap<Uuid, crate::credentials::ApiTokenRecord>,
    #[serde(default)]
    pub schedule_requests: BTreeMap<Uuid, crate::scheduling::ScheduleRequest>,
    #[serde(default, with = "super::pairs")]
    pub memory_bindings: BTreeMap<(String, VersionRef), Option<crate::memory::MemoryConfig>>,
    #[serde(default)]
    pub memory_snapshots: BTreeMap<Uuid, Vec<crate::memory::MemoryRecord>>,
    #[serde(default)]
    pub team_tasks: BTreeMap<Uuid, crate::coordination::TeamTask>,
    #[serde(default, with = "super::pairs")]
    pub coordination_policies:
        BTreeMap<(String, VersionRef), crate::coordination::CoordinationPolicy>,
    #[serde(default)]
    pub memories: BTreeMap<Uuid, crate::memory::MemoryRecord>,
    #[serde(default)]
    pub context_artifacts: BTreeMap<String, crate::context::ContextArtifact>,
    #[serde(default, with = "super::pairs")]
    pub context_policies: BTreeMap<(String, VersionRef), Option<crate::context::ContextPolicy>>,
    #[serde(default, with = "super::pairs")]
    pub model_budgets: BTreeMap<(String, String), crate::budgets::ModelBudget>,
    #[serde(default, with = "super::pairs")]
    pub model_bindings: BTreeMap<(String, VersionRef), String>,
    #[serde(with = "super::pairs")]
    pub agents: BTreeMap<(String, VersionRef), Agent>,
    #[serde(with = "super::pairs")]
    pub tools: BTreeMap<(String, VersionRef), Tool>,
    pub runs: BTreeMap<Uuid, Run>,
    pub operations: BTreeMap<Uuid, Operation>,
    pub events: BTreeMap<Uuid, Vec<Event>>,
    #[serde(with = "super::pairs")]
    pub submissions: BTreeMap<(String, String, String), Uuid>,
    #[serde(with = "super::pairs")]
    pub inputs: BTreeMap<(Uuid, String), String>,
    #[serde(with = "super::pairs")]
    pub policies: BTreeMap<(String, String), crate::security::Policy>,
}

impl Data {
    pub fn run(&self, actor: &Actor, id: Uuid) -> Result<&Run> {
        self.runs
            .get(&id)
            .filter(|r| r.meta.workspace_id == actor.workspace_id && r.actor_id == actor.id)
            .ok_or(Error::NotFound)
    }
    pub fn event(
        &mut self,
        run_id: Uuid,
        operation_id: Option<Uuid>,
        event_type: EventType,
        actor_id: Option<String>,
        payload: serde_json::Value,
    ) {
        let workspace = self.runs[&run_id].meta.workspace_id.clone();
        let events = self.events.entry(run_id).or_default();
        events.push(Event {
            meta: Metadata::new(&workspace),
            run_id,
            operation_id,
            sequence: events.len() as u64 + 1,
            event_type,
            actor_id,
            payload,
        });
    }
}

/// Clone-on-commit transactions roll back on error. Shared handles serialize writers.
/// PostgreSQL-backed handles delegate transactions to a locked database document.
#[derive(Clone, Default)]
pub struct MemoryStore {
    pub(super) data: Arc<Mutex<Data>>,
    pub(super) postgres: Option<Arc<Mutex<super::postgres::Postgres>>>,
}

impl MemoryStore {
    pub(crate) fn staging(data: Data) -> Self {
        Self {
            data: Arc::new(Mutex::new(data)),
            postgres: None,
        }
    }

    pub fn inspect_operation(&self, actor: &Actor, id: Uuid) -> Result<OperationView> {
        self.read(|d| {
            let operation = d.operations.get(&id).ok_or(Error::NotFound)?;
            d.run(actor, operation.run_id)?;
            Ok(OperationView::from(operation))
        })
    }
    pub fn operation_run(&self, actor: &Actor, id: Uuid) -> Result<Uuid> {
        self.read(|d| {
            let operation = d.operations.get(&id).ok_or(Error::NotFound)?;
            d.run(actor, operation.run_id)?;
            Ok(operation.run_id)
        })
    }
    pub(crate) fn transact<T>(&self, f: impl FnOnce(&mut Data) -> Result<T>) -> Result<T> {
        if let Some(database) = &self.postgres {
            return database
                .lock()
                .map_err(|_| Error::Conflict("store poisoned".into()))?
                .transact(f);
        }
        let mut guard = self
            .data
            .lock()
            .map_err(|_| Error::Conflict("store poisoned".into()))?;
        let mut next = guard.clone();
        let output = f(&mut next)?;
        *guard = next;
        Ok(output)
    }
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Data) -> Result<T>) -> Result<T> {
        if let Some(database) = &self.postgres {
            return database
                .lock()
                .map_err(|_| Error::Conflict("store poisoned".into()))?
                .read(f);
        }
        let guard = self
            .data
            .lock()
            .map_err(|_| Error::Conflict("store poisoned".into()))?;
        f(&guard)
    }
    pub fn inspect(&self, actor: &Actor, id: Uuid) -> Result<RunView> {
        self.read(|d| Ok(RunView::from(d.run(actor, id)?)))
    }
    /// Direct child runs only, scoped through ownership of the parent and children.
    pub fn children(&self, actor: &Actor, parent: Uuid) -> Result<Vec<RunView>> {
        self.read(|d| {
            d.run(actor, parent)?;
            let mut children = Vec::new();
            for run in d.runs.values() {
                if run
                    .parent_operation
                    .is_some_and(|id| d.operations.get(&id).is_some_and(|op| op.run_id == parent))
                {
                    d.run(actor, run.meta.id)?;
                    children.push(run);
                }
            }
            children.sort_by_key(|run| (run.meta.created_at, run.meta.id));
            Ok(children.into_iter().map(RunView::from).collect())
        })
    }
    pub fn operations(&self, actor: &Actor, id: Uuid) -> Result<Vec<Operation>> {
        self.read(|d| {
            d.run(actor, id)?;
            let mut result: Vec<_> = d
                .operations
                .values()
                .filter(|o| o.run_id == id)
                .cloned()
                .collect();
            result.sort_by_key(|o| (o.step_index, o.request_index));
            Ok(result)
        })
    }
    pub fn events(&self, actor: &Actor, id: Uuid, after: u64) -> Result<Vec<Event>> {
        self.event_page(actor, id, after, usize::MAX)
    }

    /// Read an exclusive-cursor page, checking run ownership even for empty pages.
    /// The PostgreSQL backend still loads its namespace snapshot; only selected
    /// events are cloned into the returned page.
    pub fn event_page(
        &self,
        actor: &Actor,
        id: Uuid,
        after: u64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        if limit == 0 {
            return Err(Error::Invalid("event page limit must be positive".into()));
        }
        self.read(|d| {
            d.run(actor, id)?;
            let events = d.events.get(&id).map(Vec::as_slice).unwrap_or_default();
            let start = events.partition_point(|event| event.sequence <= after);
            Ok(events[start..].iter().take(limit).cloned().collect())
        })
    }
}

impl MemoryStore {
    /// Pin the host's non-secret model transport configuration to an Agent version.
    pub fn bind_model_transport(
        &self,
        workspace: &str,
        agent: &VersionRef,
        fingerprint: &str,
    ) -> Result<()> {
        self.transact(|d| {
            let key = (workspace.to_owned(), agent.clone());
            if !d.agents.contains_key(&key) {
                return Err(Error::NotFound);
            }
            match d.model_bindings.get(&key) {
                Some(existing) if existing != fingerprint => Err(Error::Conflict(
                    "agent model transport changed; publish a new version".into(),
                )),
                Some(_) => Ok(()),
                None => {
                    d.model_bindings.insert(key, fingerprint.into());
                    Ok(())
                }
            }
        })
    }
    /// Check the immutable budget binding before a host accepts continuation controls.
    pub fn validate_model_budget(
        &self,
        actor: &Actor,
        run_id: Uuid,
        expected: &Option<crate::budgets::ModelBudgetBinding>,
    ) -> Result<()> {
        self.read(|d| {
            let run = d.run(actor, run_id)?;
            if !run.status.terminal() && &run.model_budget != expected {
                return Err(Error::Conflict("run model budget binding changed".into()));
            }
            Ok(())
        })
    }

    pub fn validate_resume(
        &self,
        actor: &Actor,
        run_id: Uuid,
        agent: &VersionRef,
        goal: &Option<Goal>,
    ) -> Result<()> {
        self.read(|d| {
            let run = d.run(actor, run_id)?;
            if &run.agent_ref != agent || &run.goal != goal {
                return Err(Error::Conflict(
                    "resume configuration differs from the saved agent or goal".into(),
                ));
            }
            Ok(())
        })
    }
}

impl MemoryStore {
    /// Link a freshly submitted child when parent and child share this store.
    /// Separate-store delegation has no local parent record and returns false.
    pub fn link_child(&self, actor: &Actor, child: Uuid, parent_operation: Uuid) -> Result<bool> {
        self.transact(|d| {
            let Some(operation) = d.operations.get(&parent_operation) else {
                return Ok(false);
            };
            let parent = d.run(actor, operation.run_id)?;
            if parent.status.terminal() || parent.status == RunStatus::Cancelling {
                return Err(Error::Conflict("parent stopped".into()));
            }
            let mut ancestor = Some(parent.meta.id);
            while let Some(id) = ancestor {
                if id == child {
                    return Err(Error::Invalid("delegation cycle".into()));
                }
                ancestor = d.runs[&id]
                    .parent_operation
                    .and_then(|op| d.operations.get(&op))
                    .map(|op| op.run_id);
            }
            let run = d.run(actor, child)?;
            if run.parent_operation == Some(parent_operation) {
                return Ok(true);
            }
            if run.parent_operation.is_some() || run.state.step != 0 {
                return Err(Error::Conflict("child already attached or started".into()));
            }
            let run = d.runs.get_mut(&child).expect("checked");
            run.parent_operation = Some(parent_operation);
            run.revision += 1;
            Ok(true)
        })
    }
}

impl MemoryStore {
    pub fn validate_child_access(
        &self,
        actor: &Actor,
        child: Uuid,
        caller_operation: Uuid,
        expected_agent: &VersionRef,
    ) -> Result<()> {
        self.read(|d| {
            let child = d.run(actor, child)?;
            let origin = child
                .parent_operation
                .and_then(|id| d.operations.get(&id))
                .ok_or(Error::Denied)?;
            let caller = d.operations.get(&caller_operation).ok_or(Error::Denied)?;
            d.run(actor, caller.run_id)?;
            if origin.run_id != caller.run_id || &child.agent_ref != expected_agent {
                return Err(Error::Denied);
            }
            Ok(())
        })
    }
}
