//! Temporal schedules the existing Hudson runtime; workflows never perform effects.
pub mod client;
pub mod scheduling;
pub use client::{ExecutionClient, RunReceipt};
pub use scheduling::SchedulingPump;

use hudson_core::{
    configured::ScheduledTree,
    models::{Actor, OperationStatus, RunStatus, RunView},
    scheduling::ScheduleTarget,
};
use std::{sync::Arc, time::Duration};
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::{
    activities::{ActivityContext, ActivityError},
    ActivityOptions, ChildWorkflowOptions, ContinueAsNewOptions, WorkflowContext, WorkflowResult,
};
use uuid::Uuid;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct TickResult {
    pub view: RunView,
    pub children: Vec<Uuid>,
}

enum ActivityRuntimes {
    Configured {
        tree: ScheduledTree,
        target: ScheduleTarget,
    },
    Published(hudson_core::published_worker::PublishedWorker),
}
impl ActivityRuntimes {
    fn store(&self) -> &hudson_core::storage::Store {
        match self {
            Self::Configured { tree, .. } => &tree.store,
            Self::Published(worker) => &worker.store,
        }
    }
    fn tick(&self, actor: &Actor, id: Uuid) -> hudson_core::Result<RunView> {
        match self {
            Self::Configured { tree, .. } => tree.tick(actor, id),
            Self::Published(worker) => worker.tick(id),
        }
    }
    fn validate_run(&self, actor: &Actor, id: Uuid) -> hudson_core::Result<()> {
        match self {
            Self::Configured { tree, target } => {
                tree.store.validate_schedule(actor, id, Some(target))
            }
            Self::Published(worker) => worker.validate_run(id).map(|_| ()),
        }
    }
}

pub struct RunActivities {
    tree: Option<ActivityRuntimes>,
    actor: Actor,
}

impl RunActivities {
    /// Bind configured activities to this worker's scheduling namespace and queue.
    /// The target comes from worker configuration, never from workflow input.
    pub fn new(tree: ScheduledTree, actor: Actor, target: ScheduleTarget) -> Self {
        Self {
            tree: Some(ActivityRuntimes::Configured { tree, target }),
            actor,
        }
    }

    fn tick_run(&self, id: Uuid) -> Result<TickResult, String> {
        let tree = self.tree.as_ref().expect("live activity tree");
        tree.validate_run(&self.actor, id)
            .map_err(|error| error.to_string())?;
        let pending = tree
            .store()
            .operations(&self.actor, id)
            .map_err(|error| error.to_string())?
            .iter()
            .any(|operation| {
                matches!(
                    operation.status,
                    OperationStatus::Pending
                        | OperationStatus::WaitingApproval
                        | OperationStatus::Running
                        | OperationStatus::Unknown
                )
            });
        let children = tree
            .store()
            .children(&self.actor, id)
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|child| !child.status.terminal())
            .map(|child| child.id)
            .collect::<Vec<_>>();
        // Finish dispatching a batch before joining its children.
        if !pending && !children.is_empty() {
            let view = tree
                .store()
                .inspect(&self.actor, id)
                .map_err(|error| error.to_string())?;
            return Ok(TickResult { view, children });
        }
        let view = tree
            .tick(&self.actor, id)
            .map_err(|error| error.to_string())?;
        Ok(TickResult {
            view,
            children: Vec::new(),
        })
    }
}
impl RunActivities {
    pub fn published(
        store: hudson_core::storage::Store,
        actor: Actor,
        target: hudson_core::scheduling::ScheduleTarget,
    ) -> hudson_core::Result<Self> {
        let worker =
            hudson_core::published_worker::PublishedWorker::new(store, actor.clone(), target)?;
        Ok(Self {
            tree: Some(ActivityRuntimes::Published(worker)),
            actor,
        })
    }
}
impl Drop for RunActivities {
    fn drop(&mut self) {
        if let Some(tree) = self.tree.take() {
            // postgres::Client performs blocking IO even during Drop. Worker
            // teardown happens inside Tokio, so close it on a plain thread.
            if tokio::runtime::Handle::try_current().is_ok() {
                let _ = std::thread::spawn(move || drop(tree)).join();
            } else {
                drop(tree);
            }
        }
    }
}

#[activities]
impl RunActivities {
    /// Runtime persists operation intent before IO and refuses to replay an
    /// already-running or unknown operation, including after activity retries.
    #[activity]
    pub async fn tick(
        self: Arc<Self>,
        ctx: ActivityContext,
        id: Uuid,
    ) -> Result<TickResult, ActivityError> {
        let mut task = tokio::task::spawn_blocking(move || self.tick_run(id));
        let mut heartbeat = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                result = &mut task => return result
                    .map_err(|e| ActivityError::from(anyhow_error(e.to_string())))?
                    .map_err(|e| ActivityError::from(anyhow_error(e))),
                _ = heartbeat.tick() => { ctx.record_heartbeat(id).await?; }
            }
        }
    }
}

fn anyhow_error(message: String) -> std::io::Error {
    std::io::Error::other(message)
}

#[cfg(test)]
mod activity_target_tests {
    use super::*;
    use hudson_core::{configured::Configuration, storage::Store};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Endpoint {
        url: String,
        calls: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Endpoint {
        fn new() -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}/model", server.server_addr());
            let calls = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let (received, done) = (calls.clone(), stop.clone());
            let thread = std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    if let Some(request) = server.recv_timeout(Duration::from_millis(50)).unwrap() {
                        received.fetch_add(1, Ordering::SeqCst);
                        request
                            .respond(tiny_http::Response::from_string("{}").with_status_code(503))
                            .unwrap();
                    }
                }
            });
            Self {
                url,
                calls,
                stop,
                thread: Some(thread),
            }
        }
    }
    impl Drop for Endpoint {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn configured_activity_checks_target_before_model_or_operation_dispatch() {
        let endpoint = Endpoint::new();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            serde_json::to_vec(&serde_json::json!({
                "name":"schedule-target-fixture", "instructions":"finish", "endpoint":endpoint.url,
            "provider":"ollama", "model":"fixture"
            }))
            .unwrap(),
        )
        .unwrap();
        let actor = Actor {
            workspace_id: "fixture".into(),
            id: "operator".into(),
        };
        let store = Store::default();
        let tree = Configuration::load(file.path())
            .unwrap()
            .build_temporal_tree(store.clone(), &actor)
            .unwrap();
        let target = ExecutionClient::schedule_target("fixture", "worker-a");
        let other = ExecutionClient::schedule_target("fixture", "worker-b");
        let id = tree
            .runtime
            .submit_scheduled(
                &actor,
                tree.reference.clone(),
                serde_json::json!("test"),
                Some("schedule-target-fixture".into()),
                tree.goal.clone(),
                Some(target.clone()),
            )
            .unwrap();
        assert!(store.validate_schedule(&actor, id, Some(&other)).is_err());
        let activities = RunActivities::new(tree.into_scheduled(), actor.clone(), other);
        for _ in 0..3 {
            let _ = activities.tick_run(id);
        }
        assert_eq!(
            endpoint.calls.load(Ordering::SeqCst),
            0,
            "wrong-queue activity reached the model"
        );
        assert!(
            store.operations(&actor, id).unwrap().is_empty(),
            "wrong-queue activity created an operation"
        );
        let tree = Configuration::load(file.path())
            .unwrap()
            .build_temporal_tree(store, &actor)
            .unwrap();
        let correct = RunActivities::new(tree.into_scheduled(), actor, target);
        for _ in 0..3 {
            let _ = correct.tick_run(id);
            if endpoint.calls.load(Ordering::SeqCst) != 0 {
                break;
            }
        }
        assert_eq!(
            endpoint.calls.load(Ordering::SeqCst),
            1,
            "matching worker did not dispatch"
        );
    }
}

#[workflow]
#[derive(Default)]
pub struct RunWorkflow;

#[workflow_methods]
impl RunWorkflow {
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>, id: Uuid) -> WorkflowResult<RunView> {
        for _ in 0..500 {
            let tick = ctx
                .execute_activity(
                    RunActivities::tick,
                    id,
                    ActivityOptions::with_start_to_close_timeout(Duration::from_secs(180))
                        .heartbeat_timeout(Duration::from_secs(10))
                        .build(),
                )
                .await?;
            let view = tick.view;
            let mut children = Vec::new();
            for child in tick.children {
                children.push(
                    ctx.start_child_workflow(
                        RunWorkflow::run,
                        child,
                        ChildWorkflowOptions::workflow_id(format!("hudson-child-{child}")),
                    )
                    .await?,
                );
            }
            // Start the entire team before waiting, so siblings can execute on
            // independent worker activities rather than nested tool callbacks.
            for child in children {
                child.result().await?;
            }
            if view.status.terminal() {
                return Ok(view);
            }
            // A durable timer also discovers approvals and input recorded by
            // the existing control API without depending on a live HTTP host.
            if matches!(view.status, RunStatus::Waiting | RunStatus::Cancelling) {
                ctx.timer(Duration::from_secs(5)).await;
            } else {
                ctx.timer(Duration::from_millis(100)).await;
            }
        }
        ctx.continue_as_new(id, ContinueAsNewOptions::default())?;
        unreachable!("continue as new exits the workflow")
    }
}

pub use run_workflow::Run as RunWorkflowDefinition;
