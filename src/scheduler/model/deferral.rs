//! Separate reference model for persisted holds and concurrent launch claims.
use super::*;
use crate::deferral::Deferral;
use crate::store::Store;

proptest! {
    #![proptest_config(model_config(256))]
    #[test]
    fn deferral_store_model(ops in deferral_store_ops(), first in deferral(), second in deferral()) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = Store::open(&path).unwrap();
        let mut task = Task::new("M-1", "model", TaskSource::Manual);
        store.insert_task(&task).unwrap();
        let mut expected: Option<Deferral> = None;
        let mut saved = task.clone();
        let mut state = TaskState::Queued;
        let mut events = 0;
        let mut now = origin();
        for op in ops {
            match op {
                0 => saved = store.get_task(task.id).unwrap().unwrap(),
                1..=3 => {
                    let value = match op { 1 => Some(first.clone()), 2 => Some(second.clone()), _ => None };
                    let result = store.set_task_deferral(task.id, value.clone());
                    if state == TaskState::Starting {
                        prop_assert!(result.is_err());
                    } else {
                        result.unwrap();
                        events += usize::from(expected != value);
                        expected = value;
                    }
                }
                4 => {
                    drop(store);
                    store = Store::open(&path).unwrap();
                }
                5 => {
                    // An old daemon snapshot must never erase a newly set hold.
                    let mut stale = saved.clone();
                    stale.state = state;
                    store.update_task(&stale).unwrap();
                }
                6 => {
                    // A candidate selected before the edit may not claim its launch.
                    let mut starting = saved.clone();
                    starting.state = TaskState::Starting;
                    let claimed = store.claim_task_start_with_target_at(&saved, &starting, None, now).unwrap();
                    let current = store.get_task(task.id).unwrap().unwrap();
                    let accepts = saved.deferral == expected && saved.state == state
                        && expected.as_ref().is_none_or(|hold| hold.until <= now);
                    prop_assert_eq!(claimed, accepts);
                    if claimed { state = TaskState::Starting; }
                    prop_assert_eq!(current.state, state);
                }
                7 => {
                    task = store.get_task(task.id).unwrap().unwrap();
                    task.state = TaskState::Queued;
                    store.update_task(&task).unwrap();
                    state = TaskState::Queued;
                }
                8 => {
                    // Audit failure rolls back the hold as well as the event.
                    let writer = rusqlite::Connection::open(&path).unwrap();
                    writer.execute_batch("CREATE TRIGGER fail_deferral BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
                    let different = Some(Deferral { until: origin(), reason: Some("rollback".into()) });
                    prop_assert!(store.set_task_deferral(task.id, different).is_err());
                    writer.execute_batch("DROP TRIGGER fail_deferral;").unwrap();
                }
                10..=12 => now = first.until + Duration::nanoseconds(i64::from(op) - 11),
                13..=15 => now = second.until + Duration::nanoseconds(i64::from(op) - 14),
                16 => now += Duration::hours(1),
                _ => {
                    let before = store.get_task(task.id).unwrap().unwrap();
                    let mut updated = before.clone();
                    updated.description = "source refresh".into();
                    store.update_source_task(&before, &updated).unwrap();
                    // Saved claims compare source content independently of the local hold.
                    saved = store.get_task(task.id).unwrap().unwrap();
                }
            }
            let current = store.get_task(task.id).unwrap().unwrap();
            prop_assert_eq!(&current.deferral, &expected);
            prop_assert_eq!(current.state, state);
            prop_assert_eq!(store.events_for_task(task.id, 1000).unwrap().len(), events);
        }
    }
}

proptest! {
    #[test]
    fn deferral_expiry_trace_preserves_other_gates(delay in 1i64..86400, gate in 0u8..6) {
        let mut world = super::policy_reload::ready_world(2, 1);
        let until = world.now + Duration::seconds(delay);
        world.step(0, 0, &Op::Defer(Some(Deferral { until, reason: Some("release".into()) })))?;
        // The lower-priority task gets the free slot; deferred work reserves nothing.
        prop_assert_eq!(world.launch_pass(1, &[])?.starts, 1);
        prop_assert_eq!(world.tasks[0].task.state, TaskState::Queued);
        prop_assert_eq!(world.tasks[1].task.state, TaskState::Running);
        world.step(2, 1, &Op::Complete(None))?;
        world.now = until - Duration::nanoseconds(1);
        prop_assert_eq!(world.launch_pass(3, &[])?.starts, 0);
        // Each gate is deliberately active at exact expiry, avoiding vacuous random walks.
        match gate {
            0 => { world.step(4, 0, &Op::Pause)?; }
            1 => world.tasks[0].task.blocked_by = vec![LinkedIssue { key: "BLOCKER".into(), title: "blocker".into(), state_type: "started".into(), pr_merged: false }],
            2 => world.scheduling_paused = true,
            3 => { for provider in crate::domain::Provider::ALL { world.rate_limits.mark_provider(&world.cfg.budget, provider, until + Duration::hours(1)); } }
            4 => world.tasks[0].task.routing = Some(crate::routing::RoutingDecision::blocked("not approved")),
            _ => world.cfg.scheduler.max_concurrent = 0,
        }
        world.now = until;
        prop_assert_eq!(world.launch_pass(5, &[])?.starts, 0);
        prop_assert_eq!(world.tasks[0].task.deferral.as_ref().unwrap().until, until);
        match gate {
            0 => { world.step(6, 0, &Op::Resume)?; }
            1 => world.tasks[0].task.blocked_by.clear(),
            2 => world.scheduling_paused = false,
            3 => { world.now = until + Duration::hours(1); }
            4 => world.tasks[0].task.routing = None,
            _ => world.cfg.scheduler.max_concurrent = 1,
        }
        prop_assert_eq!(world.launch_pass(7, &[])?.starts, 1);
        // Deferring active work keeps its live session and only affects another launch.
        let active = world.tasks[0].session.clone();
        world.step(8, 0, &Op::Defer(Some(Deferral { until: world.now + Duration::days(1), reason: None })))?;
        prop_assert_eq!(&world.tasks[0].session, &active);
        prop_assert_eq!(world.tasks[0].task.state, TaskState::Running);
        world.step(9, 0, &Op::HandOff("https://github.com/o/r/pull/1".into()))?;
        world.step(10, 0, &Op::Retry)?;
        prop_assert_eq!(world.launch_pass(11, &[])?.starts, 0);
    }
}

proptest! {
    #[test]
    fn source_edits_survive_stale_lifecycle_writes(
        descriptions in vec(deferral_description(), 1..30), local in deferral()
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = Store::open(&path).unwrap();
        let writer = Store::open(&path).unwrap();
        let task = Task::new("L-1", "source", TaskSource::Linear {
            issue_id: "id".into(), identifier: "L-1".into(), url: "url".into(), team_key: "L".into(),
        });
        store.insert_task(&task).unwrap();
        writer.set_task_deferral(task.id, Some(local.clone())).unwrap();
        for description in descriptions {
            let stale = store.get_task(task.id).unwrap().unwrap();
            let mut edit = stale.clone();
            edit.description = description;
            prop_assert!(writer.update_source_task(&stale, &edit).unwrap());
            let expected = edit.source_deferral().map_err(|e| e.to_string());
            // Simulate crash/rescore/cleanup persisting its pre-sync snapshot.
            store.update_task(&stale).unwrap();
            let reloaded = Store::open(&path).unwrap().get_task(task.id).unwrap().unwrap();
            prop_assert_eq!(reloaded.source_deferral().map_err(|e| e.to_string()), expected.clone());
            prop_assert_eq!(reloaded.deferral, Some(local.clone()));
            let mut starting = stale.clone();
            starting.state = TaskState::Starting;
            if stale.source_deferral().map_err(|e| e.to_string()) != expected {
                prop_assert!(!store.claim_task_start(&stale, &starting).unwrap());
            }
        }
    }
}

#[test]
fn persisted_future_and_invalid_holds_refuse_even_a_direct_claim() {
    let store = Store::open_in_memory().unwrap();
    let mut task = Task::new(
        "L-1",
        "source",
        TaskSource::Linear { issue_id: "id".into(), identifier: "L-1".into(), url: "url".into(), team_key: "L".into() },
    );
    task.deferral = Some(Deferral { until: Utc::now() + Duration::days(1), reason: None });
    store.insert_task(&task).unwrap();
    let mut starting = task.clone();
    starting.state = TaskState::Starting;
    assert!(!store.claim_task_start(&task, &starting).unwrap());
    task = store.set_task_deferral(task.id, None).unwrap();
    let mut edited = task.clone();
    edited.description = "Powerqueue-Defer-Until: invalid".into();
    assert!(store.update_source_task(&task, &edited).unwrap());
    starting = edited.clone();
    starting.state = TaskState::Starting;
    assert!(!store.claim_task_start(&edited, &starting).unwrap());
}

#[test]
fn migrate_v10_preserves_automatic_backoff_and_source_holds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut task = Task::new(
        "L-1",
        "legacy",
        TaskSource::Linear { issue_id: "id".into(), identifier: "L-1".into(), url: "url".into(), team_key: "L".into() },
    );
    task.state = TaskState::Crashed;
    task.not_before = Some(origin());
    task.description = "Powerqueue-Defer-Until: 2099-01-01T00:00:00Z".into();
    Store::open(&path).unwrap().insert_task(&task).unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE tasks DROP COLUMN deferral; PRAGMA user_version = 10;")
        .unwrap();
    for _ in 0..2 {
        let reopened = Store::open(&path).unwrap();
        let saved = reopened.get_task(task.id).unwrap().unwrap();
        assert_eq!(saved.not_before, task.not_before);
        assert_eq!(saved.deferral, None);
        assert_eq!(saved.description, task.description);
        assert!(!saved.deferral_allows(origin()));
        assert_eq!(reopened.schema_version().unwrap(), crate::store::SCHEMA_VERSION);
    }
}

proptest! {
    #[test]
    fn persisted_claim_checks_both_holds_at_exact_boundaries(local in deferral(), source in deferral()) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut task = Task::new("L-1", "boundary", TaskSource::Linear {
            issue_id: "id".into(), identifier: "L-1".into(), url: "url".into(), team_key: "L".into(),
        });
        task.description = format!("Powerqueue-Defer-Until: {}", source.until.to_rfc3339());
        task.deferral = Some(local.clone());
        Store::open(&path).unwrap().insert_task(&task).unwrap();
        let mut times = [local.until, source.until].into_iter()
            .flat_map(|until| [-1, 0, 1].map(|delta| until + Duration::nanoseconds(delta)))
            .collect::<Vec<_>>();
        times.sort();
        for now in times {
            // Reopen before every boundary; neither restart nor the passing
            // clock may clear either persisted hold.
            let store = Store::open(&path).unwrap();
            let before = store.get_task(task.id).unwrap().unwrap();
            let mut starting = before.clone();
            starting.state = TaskState::Starting;
            let claimed = store.claim_task_start_with_target_at(&before, &starting, None, now).unwrap();
            prop_assert_eq!(claimed, local.until <= now && source.until <= now);
            let mut after = store.get_task(task.id).unwrap().unwrap();
            prop_assert_eq!(&after.deferral, &Some(local.clone()));
            prop_assert_eq!(after.source_deferral().unwrap().unwrap().until, source.until);
            if claimed {
                prop_assert_eq!(after.state, TaskState::Starting);
                after.state = TaskState::Queued;
                store.update_task(&after).unwrap();
            } else {
                prop_assert_eq!(after, before, "refused claim mutated the persisted task");
            }
        }
    }
}
