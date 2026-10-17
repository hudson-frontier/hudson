//! Durable scheduling intent. Remote schedulers acknowledge only after accepting
//! the stable run identity; retrying publication must not replay execution.
use crate::{models::*, storage::Store, Error, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleTarget {
    pub scheduler: String,
    pub task_queue: String,
}
impl ScheduleTarget {
    pub fn validate(&self) -> Result<()> {
        if [&self.scheduler, &self.task_queue]
            .iter()
            .any(|v| v.trim().is_empty() || v.len() > 256)
        {
            return Err(Error::Invalid("invalid scheduling target".into()));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ScheduleRequest {
    pub target: ScheduleTarget,
    pub acknowledged: bool,
    #[serde(default)]
    pub last_attempt: u64,
    #[serde(default)]
    pub published_at: Option<u64>,
}
/// Scheduling publication evidence only; never proof of execution or completion.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleStatus {
    Unscheduled,
    Pending,
    Published,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleView {
    pub root_run_id: Uuid,
    pub status: ScheduleStatus,
    pub last_attempt_at: Option<u64>,
    /// Old acknowledged snapshots may have no recorded publication time.
    pub published_at: Option<u64>,
}
impl Store {
    /// Inspect the owned root's saved receipt without contacting a scheduler.
    pub fn inspect_schedule(&self, actor: &Actor, id: Uuid) -> Result<ScheduleView> {
        self.read(|data| {
            let mut root = id;
            let mut visited = std::collections::BTreeSet::new();
            loop {
                if !visited.insert(root) {
                    return Err(Error::Conflict("cyclic run ancestry".into()));
                }
                let run = data.run(actor, root)?;
                match run.parent_operation {
                    Some(operation) => {
                        root = data
                            .operations
                            .get(&operation)
                            .ok_or(Error::NotFound)?
                            .run_id
                    }
                    None => break,
                }
            }
            let request = data.schedule_requests.get(&root);
            Ok(ScheduleView {
                root_run_id: root,
                status: match request {
                    None => ScheduleStatus::Unscheduled,
                    Some(request) if request.acknowledged => ScheduleStatus::Published,
                    Some(_) => ScheduleStatus::Pending,
                },
                last_attempt_at: request.and_then(|request| {
                    (request.last_attempt != 0).then_some(request.last_attempt)
                }),
                published_at: request.and_then(|request| request.published_at),
            })
        })
    }
    /// Check that a run (or its root for delegated work) belongs to this scheduler.
    /// This is also required before accepting controls from a scheduling-only host.
    pub fn validate_schedule(
        &self,
        actor: &Actor,
        id: Uuid,
        target: Option<&ScheduleTarget>,
    ) -> Result<()> {
        self.read(|data| {
            let mut current = id;
            let mut visited = std::collections::BTreeSet::new();
            loop {
                if !visited.insert(current) {
                    return Err(Error::Conflict("cyclic run ancestry".into()));
                }
                let run = data.run(actor, current)?;
                match run.parent_operation {
                    Some(operation) => {
                        current = data
                            .operations
                            .get(&operation)
                            .ok_or(Error::NotFound)?
                            .run_id;
                    }
                    None => break,
                }
            }
            if data
                .schedule_requests
                .get(&current)
                .map(|request| &request.target)
                == target
            {
                Ok(())
            } else {
                Err(Error::Conflict(
                    "run belongs to a different execution host".into(),
                ))
            }
        })
    }

    /// Return only explicitly scheduled roots owned by this actor and agent.
    pub fn pending_schedules(
        &self,
        actor: &Actor,
        agent: &VersionRef,
        target: &ScheduleTarget,
        limit: usize,
    ) -> Result<Vec<Uuid>> {
        self.pending_schedules_matching(actor, Some(agent), target, limit)
    }

    /// Scheduled roots whose immutable publication belongs to this owner.
    pub fn pending_published_schedules(
        &self,
        actor: &Actor,
        target: &ScheduleTarget,
        limit: usize,
    ) -> Result<Vec<Uuid>> {
        self.pending_schedules_matching(actor, None, target, limit)
    }

    fn pending_schedules_matching(
        &self,
        actor: &Actor,
        agent: Option<&VersionRef>,
        target: &ScheduleTarget,
        limit: usize,
    ) -> Result<Vec<Uuid>> {
        target.validate()?;
        if !(1..=100).contains(&limit) {
            return Err(Error::Invalid(
                "scheduling batch must contain 1 to 100 runs".into(),
            ));
        }
        self.read(|data| {
            let mut pending = Vec::new();
            for (id, request) in &data.schedule_requests {
                if request.acknowledged || &request.target != target {
                    continue;
                }
                let Ok(run) = data.run(actor, *id) else {
                    continue;
                };
                let matches = match agent {
                    Some(agent) => run.agent_ref == *agent,
                    None => data
                        .publications
                        .get(&(actor.workspace_id.clone(), run.agent_ref.clone()))
                        .is_some_and(|record| record.actor_id == actor.id),
                };
                if matches && run.parent_operation.is_none() && !run.status.terminal() {
                    pending.push((request.last_attempt, run.meta.created_at, *id));
                }
            }
            pending.sort_unstable();
            Ok(pending
                .into_iter()
                .take(limit)
                .map(|(_, _, id)| id)
                .collect())
        })
    }
    /// Rotate attempted requests behind untouched work, including invalid requests.
    pub fn record_schedule_attempt(
        &self,
        actor: &Actor,
        id: Uuid,
        target: &ScheduleTarget,
    ) -> Result<()> {
        self.transact(|data| {
            data.run(actor, id)?;
            let request = data.schedule_requests.get_mut(&id).ok_or(Error::NotFound)?;
            if &request.target != target {
                return Err(Error::Conflict("scheduling target changed".into()));
            }
            request.last_attempt = now();
            Ok(())
        })
    }

    /// Trusted scheduler receipt, never exposed as a model tool.
    pub fn acknowledge_schedule(
        &self,
        actor: &Actor,
        id: Uuid,
        target: &ScheduleTarget,
    ) -> Result<()> {
        self.transact(|data| {
            data.run(actor, id)?;
            let request = data.schedule_requests.get_mut(&id).ok_or(Error::NotFound)?;
            if &request.target != target {
                return Err(Error::Conflict("scheduling target changed".into()));
            }
            if !request.acknowledged {
                request.acknowledged = true;
                request.published_at = Some(now());
            }
            Ok(())
        })
    }
}

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    use super::*;
    #[test]
    fn receipts_survive_snapshot_reopen_and_legacy_acknowledgments() {
        let runtime = crate::fixtures::runtime().unwrap();
        let actor = crate::fixtures::actor();
        let target = ScheduleTarget {
            scheduler: "temporal:test".into(),
            task_queue: "queue".into(),
        };
        let id = runtime
            .submit_scheduled(
                &actor,
                crate::fixtures::agent_ref(),
                serde_json::json!({"order_id":"123","action":"lookup"}),
                None,
                None,
                Some(target.clone()),
            )
            .unwrap();
        runtime
            .store
            .record_schedule_attempt(&actor, id, &target)
            .unwrap();
        runtime
            .store
            .acknowledge_schedule(&actor, id, &target)
            .unwrap();
        let before = runtime.store.inspect_schedule(&actor, id).unwrap();
        let mut snapshot = runtime
            .store
            .read(|data| Ok(serde_json::to_value(data)?))
            .unwrap();
        let reopened = Store::staging(serde_json::from_value(snapshot.clone()).unwrap());
        assert_eq!(reopened.inspect_schedule(&actor, id).unwrap(), before);
        let request = snapshot["schedule_requests"][id.to_string()]
            .as_object_mut()
            .unwrap();
        request.remove("published_at");
        request.remove("last_attempt");
        let legacy = Store::staging(serde_json::from_value(snapshot).unwrap());
        let receipt = legacy.inspect_schedule(&actor, id).unwrap();
        assert_eq!(receipt.status, ScheduleStatus::Published);
        assert_eq!(receipt.last_attempt_at, None);
        assert_eq!(receipt.published_at, None);
        legacy.acknowledge_schedule(&actor, id, &target).unwrap();
        assert_eq!(
            legacy.inspect_schedule(&actor, id).unwrap(),
            receipt,
            "do not invent a timestamp for legacy acknowledgment"
        );
    }
}
