#![cfg(feature = "fixtures")]
use hudson_core::{fixtures, models::Actor, scheduling::ScheduleTarget};
use serde_json::json;

#[test]
fn scheduling_intent_is_atomic_scoped_and_idempotent() {
    let runtime = fixtures::runtime().unwrap();
    let actor = fixtures::actor();
    let target = ScheduleTarget {
        scheduler: "temporal:local".into(),
        task_queue: "agents".into(),
    };
    let input = json!({"order_id":"123","action":"lookup"});
    let submit = |destination| {
        runtime.submit_scheduled(
            &actor,
            fixtures::agent_ref(),
            input.clone(),
            Some("key".into()),
            None,
            destination,
        )
    };
    let id = submit(Some(target.clone())).unwrap();
    runtime
        .store
        .validate_schedule(&actor, id, Some(&target))
        .unwrap();
    assert_eq!(submit(Some(target.clone())).unwrap(), id);
    assert!(submit(None).is_err());
    let other = ScheduleTarget {
        task_queue: "other".into(),
        ..target.clone()
    };
    assert!(submit(Some(other.clone())).is_err());
    assert!(runtime
        .store
        .validate_schedule(&actor, id, Some(&other))
        .is_err());
    assert_eq!(
        runtime
            .store
            .pending_schedules(&actor, &fixtures::agent_ref(), &target, 10)
            .unwrap(),
        vec![id]
    );
    let stranger = Actor {
        id: "stranger".into(),
        ..actor.clone()
    };
    assert!(runtime
        .store
        .pending_schedules(&stranger, &fixtures::agent_ref(), &target, 10)
        .unwrap()
        .is_empty());
    assert!(runtime
        .store
        .validate_schedule(&stranger, id, Some(&target))
        .is_err());
    assert!(runtime
        .store
        .acknowledge_schedule(&stranger, id, &target)
        .is_err());
    assert!(runtime
        .store
        .acknowledge_schedule(&actor, id, &other)
        .is_err());
    runtime
        .store
        .acknowledge_schedule(&actor, id, &target)
        .unwrap();
    runtime
        .store
        .acknowledge_schedule(&actor, id, &target)
        .unwrap();
    assert!(runtime
        .store
        .pending_schedules(&actor, &fixtures::agent_ref(), &target, 10)
        .unwrap()
        .is_empty());
    assert_eq!(submit(Some(target)).unwrap(), id);
}

#[test]
fn invalid_submissions_and_ordinary_runs_never_enter_scheduler_queue() {
    let runtime = fixtures::runtime().unwrap();
    let actor = fixtures::actor();
    let target = ScheduleTarget {
        scheduler: "temporal:local".into(),
        task_queue: "agents".into(),
    };
    assert!(runtime
        .submit_scheduled(
            &actor,
            fixtures::agent_ref(),
            json!({}),
            None,
            None,
            Some(target.clone())
        )
        .is_err());
    runtime
        .submit(
            &actor,
            fixtures::agent_ref(),
            json!({"order_id":"123","action":"lookup"}),
            None,
        )
        .unwrap();
    assert!(runtime
        .store
        .pending_schedules(&actor, &fixtures::agent_ref(), &target, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn attempted_requests_cannot_starve_untouched_work() {
    let runtime = fixtures::runtime().unwrap();
    let actor = fixtures::actor();
    let target = ScheduleTarget {
        scheduler: "temporal:local".into(),
        task_queue: "agents".into(),
    };
    for _ in 0..101 {
        runtime
            .submit_scheduled(
                &actor,
                fixtures::agent_ref(),
                json!({"order_id":"123","action":"lookup"}),
                None,
                None,
                Some(target.clone()),
            )
            .unwrap();
    }
    let pending = || {
        runtime
            .store
            .pending_schedules(&actor, &fixtures::agent_ref(), &target, 100)
            .unwrap()
    };
    let first = pending();
    assert_eq!(first.len(), 100);
    let stranger = Actor {
        id: "stranger".into(),
        ..actor.clone()
    };
    assert!(runtime
        .store
        .record_schedule_attempt(&stranger, first[0], &target)
        .is_err());
    for id in &first {
        runtime
            .store
            .record_schedule_attempt(&actor, *id, &target)
            .unwrap();
    }
    let next = pending();
    assert!(
        !first.contains(&next[0]),
        "untouched request must lead the next batch"
    );
    assert_eq!(next.len(), 100, "failed attempts must remain eligible");
}

#[test]
fn delegated_runs_inherit_the_root_execution_host() {
    let mut runtime = fixtures::runtime().unwrap();
    let actor = fixtures::actor();
    let target = ScheduleTarget {
        scheduler: "temporal:api".into(),
        task_queue: "agents".into(),
    };
    let input = json!({"order_id":"123","action":"lookup"});
    let root = runtime
        .submit_scheduled(
            &actor,
            fixtures::agent_ref(),
            input.clone(),
            None,
            None,
            Some(target.clone()),
        )
        .unwrap();
    let mut parent = root;
    for _ in 0..2 {
        runtime.tick(&actor, parent).unwrap();
        let operation = runtime.store.operations(&actor, parent).unwrap()[0].meta.id;
        let child = runtime
            .submit(&actor, fixtures::agent_ref(), input.clone(), None)
            .unwrap();
        assert!(runtime.store.link_child(&actor, child, operation).unwrap());
        runtime
            .store
            .validate_schedule(&actor, child, Some(&target))
            .unwrap();
        assert!(runtime
            .store
            .validate_schedule(&actor, child, None)
            .is_err());
        let other = ScheduleTarget {
            task_queue: "other".into(),
            ..target.clone()
        };
        assert!(runtime
            .store
            .validate_schedule(&actor, child, Some(&other))
            .is_err());
        parent = child;
    }
    runtime
        .store
        .acknowledge_schedule(&actor, root, &target)
        .unwrap();
    runtime
        .store
        .validate_schedule(&actor, parent, Some(&target))
        .unwrap();
    let receipt = runtime.store.inspect_schedule(&actor, parent).unwrap();
    assert_eq!(receipt.root_run_id, root);
    assert_eq!(
        receipt.status,
        hudson_core::scheduling::ScheduleStatus::Published
    );
    let stranger = Actor {
        workspace_id: "other-workspace".into(),
        ..actor.clone()
    };
    assert!(runtime
        .store
        .validate_schedule(&stranger, parent, Some(&target))
        .is_err());
}

#[test]
fn receipts_distinguish_publication_from_execution_and_enforce_ownership() {
    use hudson_core::scheduling::ScheduleStatus;
    let runtime = fixtures::runtime().unwrap();
    let actor = fixtures::actor();
    let input = json!({"order_id":"123","action":"lookup"});
    let local = runtime
        .submit(&actor, fixtures::agent_ref(), input.clone(), None)
        .unwrap();
    let receipt = runtime.store.inspect_schedule(&actor, local).unwrap();
    assert_eq!(receipt.status, ScheduleStatus::Unscheduled);
    assert_eq!(receipt.root_run_id, local);
    assert_eq!(receipt.last_attempt_at, None);
    assert_eq!(receipt.published_at, None);
    let target = ScheduleTarget {
        scheduler: "temporal:local".into(),
        task_queue: "private-worker-queue".into(),
    };
    let id = runtime
        .submit_scheduled(
            &actor,
            fixtures::agent_ref(),
            input,
            None,
            None,
            Some(target.clone()),
        )
        .unwrap();
    assert_eq!(
        runtime.store.inspect_schedule(&actor, id).unwrap().status,
        ScheduleStatus::Pending
    );
    runtime
        .store
        .record_schedule_attempt(&actor, id, &target)
        .unwrap();
    let attempted = runtime.store.inspect_schedule(&actor, id).unwrap();
    assert!(attempted.last_attempt_at.is_some());
    assert_eq!(attempted.published_at, None);
    runtime
        .store
        .acknowledge_schedule(&actor, id, &target)
        .unwrap();
    let published = runtime.store.inspect_schedule(&actor, id).unwrap();
    assert_eq!(published.status, ScheduleStatus::Published);
    assert!(published.published_at.is_some());
    assert_eq!(
        runtime.store.inspect(&actor, id).unwrap().status,
        hudson_core::models::RunStatus::Queued
    );
    runtime
        .store
        .acknowledge_schedule(&actor, id, &target)
        .unwrap();
    assert_eq!(
        runtime.store.inspect_schedule(&actor, id).unwrap(),
        published
    );
    runtime.cancel(&actor, id).unwrap();
    assert_eq!(
        runtime.store.inspect_schedule(&actor, id).unwrap(),
        published
    );
    assert_eq!(
        runtime.store.inspect(&actor, id).unwrap().status,
        hudson_core::models::RunStatus::Cancelled
    );
    for stranger in [
        Actor {
            id: "other".into(),
            ..actor.clone()
        },
        Actor {
            workspace_id: "other".into(),
            ..actor.clone()
        },
    ] {
        for run in [local, id] {
            assert!(runtime.store.inspect_schedule(&stranger, run).is_err());
        }
    }
    assert!(runtime
        .store
        .inspect_schedule(&actor, uuid::Uuid::new_v4())
        .is_err());
    let encoded = serde_json::to_string(&published).unwrap();
    assert!(!encoded.contains("private-worker-queue"));
    assert!(!encoded.contains("temporal:local"));
}
