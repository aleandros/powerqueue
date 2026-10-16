//! Forced launch scenarios complement the sparse arbitrary walks. Every case
//! starts real candidates before changing policy, budgets or concurrency.
use super::*;
use crate::domain::{DaemonCommand, Provider};

fn ready_world(count: usize, slots: u32) -> DaemonWorld {
    let mut cfg = Config::default();
    cfg.scheduler.max_concurrent = slots;
    // Keep pacing out of the slot/policy assertions; budget scenarios below
    // exhaust a provider through its own observed fraction instead.
    for provider in Provider::ALL {
        let budget = cfg.budget.providers.get_mut(provider);
        budget.enabled = true;
        budget.period_weighted_tokens = 1_000_000_000_000;
        budget.window_weighted_tokens = 1_000_000_000_000;
        for model in budget.models.values_mut() {
            model.min_criticality = Criticality::Low;
        }
    }
    let store = crate::store::Store::open_in_memory().unwrap();
    let tasks = (0..count)
        .map(|i| {
            let mut task = Task::new(format!("MODEL-{i}"), "model task", TaskSource::Manual);
            task.created_at = origin() - Duration::seconds((count - i) as i64);
            task.score = 100.0 - i as f64;
            store.insert_task(&task).unwrap();
            TaskWorld::new(task)
        })
        .collect();
    let mut world = DaemonWorld {
        cfg,
        rules: HashMap::new(),
        checkouts: HashMap::new(),
        now: origin(),
        tasks,
        ledgers: Ledgers::default(),
        budget_store: store,
        rate_limits: RateLimitState::default(),
        estimator: Estimator::from_summaries(&[]),
        scheduling_paused: false,
        launch_failures: Default::default(),
    };
    world.set_project("default", &(true, false, ModelTier::sonnet()));
    world.refresh_ledgers();
    world
}

fn checked(world: &mut DaemonWorld, step: usize, op: DaemonOp) -> Result<(), TestCaseError> {
    let before = world.clone();
    prop_assert!(world.apply(step, &op)?);
    check_daemon(step, &op, &before, world)
}

proptest! {
    #![proptest_config(model_config(32))]

    #[test]
    fn lowered_concurrency_drains_existing_sessions(high in 2u32..=4, low in 1u32..=3) {
        let low = low.min(high - 1);
        let mut world = ready_world(high as usize + 1, high);
        prop_assert_eq!(world.launch_pass(0, &[])?.starts, high);
        checked(&mut world, 1, DaemonOp::ReloadConcurrency(low))?;
        prop_assert_eq!(world.live_count(), high, "reload must not kill sessions");
        for i in 0..high {
            let live = world.live_count();
            let expected = u32::from(live < low && world.tasks[high as usize].can_start(world.now));
            let pass = world.launch_pass(2 + i as usize, &[])?;
            prop_assert_eq!(pass.starts, expected, "capacity only returns below the new limit");
            world.step(10 + i as usize, i as usize, &Op::Complete(None))?;
        }
        world.launch_pass(20, &[])?;
        prop_assert_eq!(world.live_count(), 1, "a waiting task must eventually launch");
    }

    #[test]
    fn conditional_preferences_reload_between_launches(document in conditional_model_policy(), label in any::<bool>()) {
        let mut world = ready_world(3, 1);
        prop_assert_eq!(world.launch_pass(0, &[])?.starts, 1);
        let active = world.tasks[0].clone();
        checked(&mut world, 1, DaemonOp::SpecialLabel(1, label))?;
        checked(&mut world, 2, DaemonOp::ReloadPolicy("default".into(), document.clone()))?;
        prop_assert_eq!(&world.tasks[0].task, &active.task, "reload rewrote active work");
        prop_assert_eq!(&world.tasks[0].session, &active.session);
        let rules = PriorityRules::parse(&document).unwrap();
        let evaluation = rules.evaluate(&world.tasks[1].task, world.now, None, 0.0, 0.0);
        prop_assert_eq!(&world.tasks[1].task.score_reasons, &evaluation.reasons, "reload lost the model preference trail");
        let expected = evaluation.models;
        prop_assert!(!expected.is_empty());
        world.step(3, 0, &Op::Complete(None))?;
        let pass = world.launch_pass(4, &[])?;
        prop_assert_eq!(pass.starts, 1);
        prop_assert_eq!(world.tasks[1].task.model.as_ref(), expected.first(), "conditional preference did not reach the launch");
        checked(&mut world, 5, DaemonOp::SpecialLabel(2, !label))?;
        world.step(6, 1, &Op::Complete(None))?;
        let expected = rules.evaluate(&world.tasks[2].task, world.now, None, 0.0, 0.0).models;
        prop_assert_eq!(world.launch_pass(7, &[])?.starts, 1);
        prop_assert_eq!(world.tasks[2].task.model.as_ref(), expected.first());
    }

    #[test]
    fn dated_usage_and_observations_survive_launch_passes(tokens in 1u64..1_000_000, age in 2i64..5 * 3600, reset in any::<bool>()) {
        let mut world = ready_world(2, 1);
        prop_assert_eq!(world.launch_pass(0, &[])?.starts, 1);
        checked(&mut world, 1, DaemonOp::Tick(5 * 3600))?;
        checked(&mut world, 2, DaemonOp::SpendRecorded { task: 0, tokens, age_secs: age })?;
        checked(&mut world, 3, DaemonOp::SpendRecorded { task: 0, tokens, age_secs: 1 })?;
        prop_assert_eq!(world.ledgers.get(Provider::Claude).unwrap().total_window_weighted, 2.0 * tokens as f64);
        // Only the older record rolls out. The previous approximate model
        // retained both until an entire window had passed between refreshes.
        checked(&mut world, 4, DaemonOp::Tick(5 * 3600 - age + 1))?;
        world.refresh_ledgers();
        let recorded_at = world.now;
        let before = world.ledgers.get(Provider::Claude).unwrap().clone();
        prop_assert_eq!(before.total_window_weighted, tokens as f64);
        prop_assert_eq!(before.total_period_weighted, 2.0 * tokens as f64);
        world.step(5, 0, &Op::Complete(None))?;
        // Disable alternatives so exhaustion must throttle rather than switch providers.
        world.cfg.budget.providers.codex.enabled = false;
        world.cfg.budget.providers.gemini.enabled = false;
        checked(&mut world, 3, DaemonOp::Observation(Provider::Claude, 1.0, reset))?;
        let pass = world.launch_pass(4, &[])?;
        prop_assert_eq!(pass.starts, 0);
        prop_assert_eq!(world.tasks[1].task.state, TaskState::Throttled);
        let retry = world.tasks[1].task.not_before.unwrap();
        world.now = retry - Duration::seconds(1);
        prop_assert!(world.launch_pass(5, &[])?.handed.is_empty(), "retry deadline bypassed");
        world.now = recorded_at + Duration::hours(5) + Duration::seconds(1);
        world.refresh_ledgers();
        let after = world.ledgers.get(Provider::Claude).unwrap();
        prop_assert_eq!(after.total_window_weighted, 0.0, "dated spend did not expire");
        prop_assert_eq!(after.total_period_weighted, before.total_period_weighted);
        prop_assert_eq!(world.launch_pass(6, &[])?.starts, 1, "expired observation kept the queue blocked");
        world.now += Duration::days(7);
        world.refresh_ledgers();
        prop_assert_eq!(world.ledgers.get(Provider::Claude).unwrap().total_period_weighted, 0.0);
    }

    #[test]
    fn offline_writers_preserve_live_session_ownership(
        ops in vec(offline_control(), 1..20)
    ) {
        let mut world = ready_world(1, 1);
        prop_assert_eq!(world.launch_pass(0, &[])?.starts, 1);
        let mut task = world.tasks[0].task.clone();
        let session = world.tasks[0].session.clone().unwrap();
        let store = crate::store::Store::open_in_memory().unwrap();
        store.insert_task(&task).unwrap();
        store.insert_session(&session).unwrap();
        for op in ops {
            let id = task.id;
            let cmd = match op {
                0 => DaemonCommand::Pause { task_id: id },
                1 => DaemonCommand::Resume { task_id: id },
                2 => DaemonCommand::Cancel { task_id: id },
                _ => DaemonCommand::Retry { task_id: id },
            };
            crate::cli::commands::task::apply_offline(&store, &mut task, &cmd).unwrap();
            task = store.get_task(id).unwrap().unwrap();
            prop_assert!(task.state.may_keep_session(), "offline command orphaned a live session: {:?}", cmd);
            prop_assert_eq!(task.attempts, session.attempt);
            prop_assert_eq!(store.get_session(session.id).unwrap().unwrap(), session.clone());
        }
    }

    #[test]
    fn latest_session_follows_execution_after_attempt_reset(
        old_attempt in 2u32..10, delay in 0i64..=1
    ) {
        let mut world = ready_world(1, 1);
        prop_assert_eq!(world.launch_pass(0, &[])?.starts, 1);
        let mut task = world.tasks[0].task.clone();
        let session = world.tasks[0].session.clone().unwrap();
        let store = crate::store::Store::open_in_memory().unwrap();
        task.state = TaskState::Failed;
        task.completed_at = Some(origin());
        task.attempts = old_attempt;
        store.insert_task(&task).unwrap();
        let mut old = session.clone();
        old.attempt = old_attempt;
        old.state = SessionState::Exited;
        old.started_at = origin();
        store.insert_session(&old).unwrap();
        let id = task.id;
        crate::cli::commands::task::apply_offline(&store, &mut task, &DaemonCommand::Retry { task_id: id }).unwrap();
        prop_assert_eq!(task.attempts, 0);
        // Drive the same start/claim/resume-plan/launched transitions as a pass.
        let mut world = TaskWorld::new(task);
        world.session = Some(old);
        let model = ModelTier::sonnet();
        let decision = Decision {
            model: Some(model.clone()), retry_at: None,
            prediction: crate::budget::Prediction::default_guess(), reasons: vec![],
        };
        let cfg = Config::default();
        let now = origin() + Duration::seconds(delay);
        for (step, op) in [Op::Start(model, decision), Op::Claim, Op::Launched { answered: false }].iter().enumerate() {
            let before = world.clone();
            let effects = world.apply(&cfg, now, op).unwrap();
            check(step, op, &cfg, &before, &world, &effects)?;
        }
        let mut current = world.session.unwrap();
        current.tmux_window = "@current".into();
        prop_assert_eq!(current.attempt, 1);
        store.insert_session(&current).unwrap();
        store.update_task(&world.task).unwrap();
        prop_assert_eq!(store.latest_session(id).unwrap().unwrap().id, current.id,
            "historical high attempt hid the new live session");
        prop_assert_eq!(store.list_sessions_for_task(id).unwrap().len(), 2);
        let sample = crate::domain::ResourceSample {
            session_id: current.id, task_id: id, timestamp: now,
            cpu_percent: 25.0, rss_bytes: 4096, process_count: 1,
        };
        store.record_resource_sample(&sample).unwrap();
        let snapshot = crate::dashboard::Snapshot::load(&store, &cfg, now).unwrap();
        prop_assert_eq!(snapshot.latest_session(id).unwrap().id, current.id);
        prop_assert_eq!(&snapshot.latest_session(id).unwrap().tmux_window, "@current");
        prop_assert_eq!(snapshot.resources.get(&id), Some(&sample));
    }
}
