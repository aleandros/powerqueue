//! Stateful queue/workflow coverage using the production planner and transitions.
//! The capacity oracle counts sessions independently; restart reloads real SQLite rows.
use super::*;
use crate::execution::{Completion, Execution, TriageOutcome};
use crate::strategies::TriageModelOp;

struct World {
    daemon: DaemonWorld,
    bindings: Vec<Execution>,
}

impl World {
    fn new(coding: u32, triage: u32) -> Self {
        let mut daemon = super::policy_reload::ready_world(8, coding + triage);
        let config = crate::execution::tests::config();
        daemon.cfg.queues = config.queues;
        daemon.cfg.workflows = config.workflows;
        daemon.cfg.queues.get_mut("default").unwrap().max_concurrent = coding;
        daemon.cfg.queues.get_mut("triage").unwrap().max_concurrent = triage;
        for (i, row) in daemon.tasks.iter_mut().enumerate() {
            if i >= 4 {
                row.task.source = TaskSource::Linear {
                    issue_id: row.task.id.to_string(),
                    identifier: format!("IT-{i}"),
                    url: format!("https://linear.app/IT-{i}"),
                    team_key: "IT".into(),
                };
            }
            transitions::on_execution_selected(&mut row.task, &daemon.cfg).unwrap();
        }
        let bindings = daemon.tasks.iter().map(|t| t.task.execution.clone().unwrap()).collect();
        Self { daemon, bindings }
    }

    #[expect(clippy::as_conversions, reason = "Fixture counts and timestamps are bounded to small values by the test inputs.")]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Fixture counts and timestamps are bounded to small values by the test inputs."
    )]
    fn count(&self, queue: &str) -> u32 {
        self.daemon.tasks.iter().filter(|t| t.session_live() && t.task.execution.as_ref().unwrap().queue == queue).count() as u32
    }

    #[expect(clippy::as_conversions, reason = "Fixture counts and timestamps are bounded to small values by the test inputs.")]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Fixture counts and timestamps are bounded to small values by the test inputs."
    )]
    #[expect(clippy::too_many_lines, reason = "Keep the apply workflow and its early exits together in execution order.")]
    fn apply(&mut self, step: usize, op: &TriageModelOp) -> Result<(), TestCaseError> {
        let d = &mut self.daemon;
        d.now += Duration::seconds(1);
        let before = d.tasks.clone();
        match op {
            TriageModelOp::Launch(fail) => {
                let outcomes = if *fail {
                    vec![LaunchOutcome::Failed("workflow missing".into(), super::super::launch_failures::FailureScope::Task)]
                } else {
                    vec![]
                };
                // Also checks store-order independence and budget reservations.
                d.apply(step, &DaemonOp::LaunchPass(outcomes))?;
            }
            TriageModelOp::Complete(i, outcome) => {
                let row = &mut d.tasks[*i];
                let old = row.task.clone();
                if *i >= 4 {
                    let result = commands::on_triage_complete(&mut row.task, "Full investigation", *outcome, d.now);
                    let allowed = matches!(old.state, TaskState::Running | TaskState::Crashed | TaskState::Failed);
                    prop_assert_eq!(result.is_ok(), allowed);
                    if allowed {
                        prop_assert_eq!(row.task.execution.as_ref().unwrap().outcome, Some(*outcome));
                        prop_assert_eq!(row.task.completed_at, Some(d.now));
                        prop_assert_eq!(&row.session, &before[*i].session, "completion must await session release");
                        prop_assert!(result.unwrap().iter().all(|e| matches!(e, Effect::Log { .. })), "completion must only log");
                    } else {
                        prop_assert_eq!(&row.task, &old, "refused completion mutated the task");
                    }
                } else {
                    commands::on_complete(&mut row.task, Some("Coding done"), d.now).ok();
                }
            }
            TriageModelOp::Done(i) => {
                let row = &mut d.tasks[*i];
                let old = row.task.clone();
                row.apply(&d.cfg, d.now, &Op::Hook(HookOutcome::Completed { summary: "Short marker".into() }));
                if *i >= 4 {
                    prop_assert_eq!(&row.task.summary, &old.summary);
                    prop_assert_eq!(row.task.completed_at, old.completed_at);
                    if old.execution.as_ref().unwrap().outcome.is_none() {
                        prop_assert_eq!(&row.task, &old, "bare done bypassed report recording");
                    }
                }
            }
            TriageModelOp::Retry(i) => {
                d.step(step, *i, &Op::Retry)?;
                if before[*i].task.state.is_terminal() {
                    prop_assert_eq!(d.tasks[*i].task.state, TaskState::Queued);
                    prop_assert!(d.tasks[*i].task.execution.as_ref().unwrap().outcome.is_none());
                    prop_assert!(d.tasks[*i].task.summary.is_none());
                }
            }
            TriageModelOp::Release(i) => {
                d.tasks[*i].finalize(d.now);
            }
            TriageModelOp::Crash(i) => {
                let row = &mut d.tasks[*i];
                if row.session_live() && !row.task.state.is_terminal() {
                    transitions::on_crash(&mut row.task, row.session.as_mut(), "agent died", None, &d.cfg.scheduler, d.now, None);
                }
            }
            TriageModelOp::Restart => {
                for row in &mut d.tasks {
                    d.budget_store.update_task(&row.task).unwrap();
                    row.task = d.budget_store.get_task(row.task.id).unwrap().unwrap();
                    row.apply(&d.cfg, d.now, &Op::Restart);
                }
            }
            TriageModelOp::Reload { coding, triage, presence } => {
                d.cfg.queues = crate::execution::tests::config().queues;
                d.cfg.queues.get_mut("default").unwrap().max_concurrent = *coding;
                d.cfg.queues.get_mut("triage").unwrap().max_concurrent = *triage;
                d.cfg.scheduler.max_concurrent = coding + triage;
                if *presence == 1 {
                    d.cfg.queues.remove("triage");
                }
                if *presence == 2 {
                    d.cfg.queues.clear();
                }
                // Existing sessions/retries must retain their saved completion policy.
                let policy = d.cfg.workflows.get_mut("triage").unwrap();
                policy.completion = Completion::Standard;
                policy.instructions = "changed-coding-workflow.md".into();
            }
            TriageModelOp::Relabel(i, value) => {
                d.tasks[*i].task.labels = if *value { vec!["triage/requested".into()] } else { vec![] };
                for row in &mut d.tasks {
                    transitions::on_execution_selected(&mut row.task, &d.cfg).unwrap();
                }
            }
            TriageModelOp::Tick(secs) => {
                d.now += Duration::seconds(*secs);
            }
        }
        // Inspect every row after every operation, including unsuccessful commands.
        for (i, row) in d.tasks.iter().enumerate() {
            if matches!(op, TriageModelOp::Restart | TriageModelOp::Reload { .. }) {
                let mut expected = before[i].task.clone();
                // Store::update_task writes its own millisecond timestamp.
                if matches!(op, TriageModelOp::Restart) {
                    expected.updated_at = row.task.updated_at;
                }
                prop_assert_eq!(&row.task, &expected, "restart/reload changed task");
                prop_assert_eq!(&row.session, &before[i].session, "restart/reload changed session");
            }
            let mut binding = row.task.execution.clone().unwrap();
            binding.outcome = None;
            prop_assert_eq!(&binding, &self.bindings[i], "workflow changed across {:?}", op);
            if i >= 4 {
                prop_assert!(row.task.leaves_ticket_open());
                prop_assert!(row.task.pr_url.is_none());
                if row.task.state == TaskState::Completed {
                    prop_assert!(row.task.execution.as_ref().unwrap().outcome.is_some());
                    prop_assert_eq!(row.task.summary.as_deref(), Some("Full investigation"));
                }
                let mut refused = row.task.clone();
                prop_assert!(commands::on_hand_off(&mut refused, "https://github.com/o/r/pull/1", None, d.now).is_err());
                prop_assert_eq!(&refused, &row.task, "triage escaped into PR lifecycle");
            }
            if let Some(session) = &row.session {
                prop_assert_eq!(session.task_id, row.task.id);
                if session.state.is_live() {
                    prop_assert_eq!(session.attempt, row.task.attempts);
                }
            }
        }
        for queue in ["default", "triage"] {
            let prior =
                before.iter().filter(|t| t.session_live() && t.task.execution.as_ref().unwrap().queue == queue).count() as u32;
            let limit = d.cfg.queues.get(queue).map_or_else(
                || {
                    if d.cfg.queues.is_empty() && queue == "default" { d.cfg.scheduler.max_concurrent } else { 0 }
                },
                |q| q.max_concurrent,
            );
            let current =
                d.tasks.iter().filter(|t| t.session_live() && t.task.execution.as_ref().unwrap().queue == queue).count() as u32;
            prop_assert!(current <= prior.max(limit), "reload/launch exceeded dedicated capacity");
        }
        Ok(())
    }
}

proptest! {
    #![proptest_config(model_config(128))]
    #[test]
    fn queues_and_triage_survive_lifecycle_and_reload_sequences(ops in vec(triage_model_op(), 1..model_steps())) {
        let mut world = World::new(1, 1);
        // Force meaningful coverage before the arbitrary walk, including a
        // missing queue while a coding session still occupies its own slot.
        let prefix = [
            TriageModelOp::Launch(false), TriageModelOp::Done(4),
            TriageModelOp::Complete(4, TriageOutcome::NeedsInformation), TriageModelOp::Done(4),
            TriageModelOp::Retry(4), TriageModelOp::Restart,
            TriageModelOp::Reload { coding: 1, triage: 1, presence: 1 }, TriageModelOp::Launch(false),
            TriageModelOp::Reload { coding: 1, triage: 1, presence: 0 }, TriageModelOp::Launch(false),
        ];
        for (step, op) in prefix.iter().chain(&ops).enumerate() { world.apply(step, op)?; }
    }

    #[test]
    #[expect(clippy::as_conversions, reason = "Fixture counts and timestamps are bounded to small values by the test inputs.")]
    fn dedicated_limits_drain_by_attrition_and_failures_do_not_borrow_slots(high in 2u32..=3) {
        let mut world = World::new(high, high);
        world.apply(0, &TriageModelOp::Launch(true))?;
        prop_assert_eq!(world.count("default"), high, "failed coding start consumed capacity");
        prop_assert_eq!(world.count("triage"), high, "coding failures blocked triage");
        world.apply(1, &TriageModelOp::Reload { coding: 1, triage: 1, presence: 0 })?;
        world.apply(2, &TriageModelOp::Launch(false))?;
        prop_assert_eq!(world.count("triage"), high, "reload killed sessions");
        for i in 4..4 + high as usize {
            world.apply(i * 3, &TriageModelOp::Complete(i, TriageOutcome::ReadyForAgent))?;
            world.apply(i * 3 + 1, &TriageModelOp::Release(i))?;
        }
        world.apply(20, &TriageModelOp::Launch(false))?;
        // The global limit is still occupied by surviving coding sessions.
        prop_assert_eq!(world.count("triage"), 0);
        for i in 1..high as usize {
            world.apply(21 + i * 2, &TriageModelOp::Complete(i, TriageOutcome::ManualAction))?;
            world.apply(22 + i * 2, &TriageModelOp::Release(i))?;
        }
        world.apply(30, &TriageModelOp::Launch(false))?;
        prop_assert_eq!(world.count("default"), 1);
        prop_assert_eq!(world.count("triage"), 1, "free triage capacity was starved by queued coding work");
    }
}
