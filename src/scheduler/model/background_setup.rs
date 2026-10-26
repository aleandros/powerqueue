//! Workers are explicit across passes: slot/queue occupancy is counted from
//! pending workers plus live sessions, independently of the production planner.
use super::*;
use crate::strategies::SetupModelOp;

struct World {
    daemon: DaemonWorld,
    pending: HashMap<usize, (launch::Candidate, bool)>,
}

impl World {
    fn new() -> Self {
        let mut daemon = super::policy_reload::ready_world(6, 3);
        let queues = crate::execution::tests::config();
        daemon.cfg.queues = queues.queues;
        daemon.cfg.workflows = queues.workflows;
        for (i, row) in daemon.tasks.iter_mut().enumerate() {
            if i >= 3 {
                row.task.source = crate::execution::tests::it_task().source;
            }
            transitions::on_execution_selected(&mut row.task, &daemon.cfg).unwrap();
        }
        Self { daemon, pending: HashMap::new() }
    }

    fn occupied(&self, queue: Option<&str>) -> usize {
        self.daemon
            .tasks
            .iter()
            .enumerate()
            .filter(|(i, row)| {
                (row.session_live() || self.pending.contains_key(i))
                    && queue.is_none_or(|q| row.task.execution.as_ref().unwrap().queue == q)
            })
            .count()
    }

    #[expect(clippy::as_conversions, reason = "Fixture counts and timestamps are bounded to small values by the test inputs.")]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Fixture counts and timestamps are bounded to small values by the test inputs."
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "The test or replay driver consumes each input operation before advancing to the next one."
    )]
    #[expect(clippy::too_many_lines, reason = "Keep the apply workflow and its early exits together in execution order.")]
    fn apply(&mut self, op: SetupModelOp) -> Result<(), TestCaseError> {
        use SetupModelOp::*;
        self.daemon.now += Duration::seconds(60);
        let now = self.daemon.now;
        match op {
            Pass => {
                let d = &self.daemon;
                let live: Vec<_> = d.tasks.iter().filter(|t| t.session_live()).map(|t| t.task.clone()).collect();
                let tasks = d
                    .tasks
                    .iter()
                    .enumerate()
                    .filter(|(i, t)| !t.session_live() && !self.pending.contains_key(i))
                    .map(|(_, t)| t.task.clone())
                    .collect();
                let pending: Vec<_> = self.pending.values().map(|(c, _)| c.clone()).collect();
                let mut planner = LaunchPlanner::new(
                    d.estimator.clone(),
                    d.ledgers.clone(),
                    tasks,
                    d.cfg.scheduler.max_concurrent.saturating_sub(live.len() as u32),
                )
                .with_queues(&d.cfg, &live)
                .with_pending(&d.cfg.budget, &pending);
                while let Some(mut c) =
                    planner.next_for_projects(&self.daemon.cfg.budget, &self.daemon.rules, &self.daemon.rate_limits, now)
                {
                    let i = self.daemon.tasks.iter().position(|t| t.task.id == c.task.id).unwrap();
                    prop_assert!(!self.pending.contains_key(&i), "second worker for the same task");
                    let queue = c.task.execution.as_ref().unwrap().queue.clone();
                    prop_assert!(self.occupied(None) < self.daemon.cfg.scheduler.max_concurrent as usize);
                    prop_assert!(self.occupied(Some(&queue)) < self.daemon.cfg.queues[&queue].max_concurrent as usize);
                    let model = c.decision.model.clone().expect("generous model budget");
                    let d = &mut self.daemon;
                    let row = &mut d.tasks[i];
                    prop_assert!(row.apply(&d.cfg, now, &Op::Start(model, c.decision.clone())).is_some());
                    prop_assert!(row.apply(&d.cfg, now, &Op::Claim).is_some());
                    c.task = row.task.clone();
                    planner.started(&d.cfg.budget, &c);
                    self.pending.insert(i, (c, true));
                }
            }
            Finish(i, success) => {
                if let Some((claimed, owned)) = self.pending.remove(&i) {
                    let d = &mut self.daemon;
                    let row = &mut d.tasks[i];
                    let before = row.task.clone();
                    if launch::setup_is_current(&claimed.task, &row.task) {
                        if owned {
                            let op =
                                if success { Op::Launched { answered: false } } else { Op::LaunchFailed("setup failed".into()) };
                            prop_assert!(row.apply(&d.cfg, now, &op).is_some());
                            prop_assert_eq!(row.session_live(), success);
                        } else {
                            launch::on_abandoned_start(
                                &mut row.task,
                                &[],
                                launch::StartActivity::Abandoned,
                                &d.cfg.scheduler,
                                now,
                            );
                            prop_assert!(matches!(row.task.state, TaskState::Crashed | TaskState::Failed));
                        }
                    } else {
                        prop_assert_eq!(&row.task, &before, "stale completion overwrote a command");
                        prop_assert!(!row.session_live(), "stale completion launched an agent");
                    }
                    row.start = None;
                    row.claimed = false;
                }
            }
            Pause(i) => {
                let r = &mut self.daemon.tasks[i];
                let live = r.session_live();
                commands::on_pause(&mut r.task, live);
            }
            Resume(i) => {
                let r = &mut self.daemon.tasks[i];
                let live = r.session_live();
                commands::on_resume(&mut r.task, live, now);
            }
            Cancel(i) => {
                let r = &mut self.daemon.tasks[i];
                commands::on_cancel(&mut r.task, r.session.as_mut(), now);
            }
            Retry(i) => {
                let r = &mut self.daemon.tasks[i];
                commands::on_retry(&mut r.task, r.session.as_mut(), now);
            }
            Restart => {
                for (c, owned) in self.pending.values_mut() {
                    *owned = false;
                    let row = self.daemon.tasks.iter_mut().find(|r| r.task.id == c.task.id).unwrap();
                    let before = row.task.clone();
                    launch::on_abandoned_start(
                        &mut row.task,
                        &[],
                        launch::StartActivity::LiveSetup,
                        &self.daemon.cfg.scheduler,
                        now,
                    );
                    prop_assert_eq!(&row.task, &before, "restart recovered a surviving setup");
                }
            }
            Capacity(coding, triage) => {
                self.daemon.cfg.scheduler.max_concurrent = coding + triage;
                self.daemon.cfg.queues.get_mut("default").unwrap().max_concurrent = coding;
                self.daemon.cfg.queues.get_mut("triage").unwrap().max_concurrent = triage;
            }
        }
        for i in self.pending.keys() {
            prop_assert!(!self.daemon.tasks[*i].session_live(), "setup and session double-own a slot");
        }
        Ok(())
    }
}

proptest! {
    #![proptest_config(model_config(128))]
    #[test]
    fn setup_interleaves_with_commands_restart_and_queue_capacity(ops in setup_model_ops()) {
        use SetupModelOp::*;
        let mut w = World::new();
        // Force long setup, cancellation/retry, another pass, success, failure
        // and restart; the random suffix explores further interleavings.
        for op in [Pass, Cancel(0), Retry(0), Pass, Finish(0, true), Pass,
            Finish(1, false), Finish(3, true), Pass, Restart, Finish(0, true), Pass]
            .into_iter().chain(ops)
        {
            w.apply(op)?;
        }
    }
}
