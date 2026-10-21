//! Model-based tests of the daemon (proptest).
//!
//! Two models share the ops, the generators and the invariant checks.
//!
//! **One task.** A random sequence of the calls the daemon makes into the
//! pure core (commands, launches, hooks, probes, the PR watcher,
//! re-scoring) and the writes the CLI makes straight into the store (`task
//! complete`, `task block`) is run against one task, with the preconditions
//! of each call mirroring its call site. After every step the task, its
//! session and the effects are checked against invariants no single
//! transition test can see: a live session only exists in states that may
//! keep one, `not_before` is only set while crashed or throttled, attempts
//! move only through `claim` and a retry, terminal states are only left by
//! a retry (or a late done marker of a failed session), a launch under way
//! is never interrupted by a direct write, review rounds stay within
//! `review_rounds_max`, and every effect serialises and deserialises to
//! itself (the trace a specification would consume).
//!
//! **The whole daemon.** Several tasks share slots, ledgers and rate-limit
//! marks, and launches happen only through a *launch pass* that mirrors
//! `Daemon::launch_tasks`: slots from `max_concurrent` minus the live
//! sessions, the real [`LaunchPlanner`] over the open tasks without a live
//! session, each candidate throttled or started exactly as `start_task`
//! does, with a generated outcome deciding whether the start launches or
//! fails before it. Usage recorded against a live session, the scheduling
//! pause, time passing and the rate-limit marks the hooks and probes
//! produce feed later passes. On top of the per-task invariants, every step
//! checks that a pass never creates more live sessions than `max_concurrent`
//! allows, and every pass that it hands out each ready task at most once,
//! that a throttled candidate gets the policy's retry time and no slot,
//! that a failed start frees its slot, that every ledger the pass reserved
//! on stays within the safety margin, that no ready task is left
//! unconsidered while global and selected-queue slots are free (bounded progress), and that the
//! order the store lists the tasks in does not change the pass.
//!
//! Projects have independent model preferences, availability and checkout revisions.
//! Reassignment and checkout checks interleave with lifecycle operations; rejected
//! commands preserve the entire task/session. Configuration changes cannot rewrite
//! bindings or other tasks, missing projects cannot launch, and mismatched bindings
//! consume no slot or reservation. A separate SQLite model interleaves saved
//! candidates, reassignment and conditional launch claims for two tasks.
//!
//! GitHub tasks carry missing, denied or approved admission. Admission changes
//! interleave with setup and active work; setup revocation holds the launch without
//! consuming a slot/reservation. Restarts preserve durable admission. A SQLite model
//! additionally checks claims against stale approval and material-issue snapshots.
//! Source mapping decisions interleave with lifecycle operations and launch setup:
//! conflicts deny future starts without retargeting tasks, sessions or artifacts.
//! A SQLite recovery model interleaves abandoned-start observations with injected
//! write failures, restarts, project changes, later attempts/completion and session
//! writes, checking that recovery never consumes an extra attempt or replaces
//! a superseding task/session.
//!
//! Policy/reload operations include conditional preferences, label updates and
//! concurrency changes. Lowered capacity drains existing sessions without killing
//! them. Dated usage and window observations go through SQLite and the production
//! ledger loader; reservations stay local to a pass. `policy_reload` forces
//! non-vacuous launch sequences and exercises offline CLI writers and historical
//! session selection. See CONTRIBUTING.md for stress knobs and explicit limits.
//!
//! The ops are generated without preconditions and skipped when one does
//! not hold, which keeps the generator simple and lets shrinking drop any
//! op.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;

use crate::budget::{Decision, Estimator, Ledgers, RateLimitState, WINDOW_RECHECK};
use crate::config::{Config, SchedulerConfig};
use crate::domain::{Criticality, LinkedIssue, ModelTier, Session, SessionState, Task, TaskId, TaskSource, TaskState};
use crate::github::PrStatus;
use crate::priority::{Evaluation, PriorityRules};
use crate::session::{HookOutcome, SessionProbe};
use crate::strategies::*;

use super::launch::LaunchPlanner;
use super::transitions::{Effect, ProbeContext};
use super::{commands, launch, review, transitions};

proptest! {
    /// Real SQLite transactions, a separate writer connection, and reopenings
    /// exercise the gap between observing a start and committing its recovery.
    #[test]
    fn starting_recovery_survives_failures_restarts_and_stale_observations(
        (mut task, mut session) in task_with_session(),
        ops in start_recovery_ops(),
    ) {
        use StartRecoveryOp::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = crate::store::Store::open(&path).unwrap();
        let writer = rusqlite::Connection::open(&path).unwrap();
        task.state = TaskState::Starting;
        task.attempts = 1;
        task.max_attempts = Some(100);
        task.review = None;
        store.insert_task(&task).unwrap();
        let mut version = 0usize;
        let mut live = false;
        let mut inserted = false;
        let mut saved = None;
        let cfg = SchedulerConfig::default();
        for op in ops {
            let current = store.get_task(task.id).unwrap().unwrap();
            match op {
                Observe => {
                    saved = Some((current.clone(), store.list_sessions_for_task(task.id).unwrap(), version, live));
                }
                Recover | FailRecovery => {
                    if let Some((snapshot, sessions, observed_version, observed_live)) = &saved {
                        let mut recovered = snapshot.clone();
                        let effects = launch::on_abandoned_start(&mut recovered, sessions, launch::StartActivity::Abandoned, &cfg, origin());
                        let transition = snapshot.state == TaskState::Starting && !observed_live;
                        prop_assert_eq!(recovered != *snapshot, transition);
                        if transition {
                            // Reversible metadata changes can return to an
                            // identical row within SQLite's timestamp precision.
                            // Such an equivalent observation is still valid.
                            let accepts = (*observed_version == version || *snapshot == current) && !live;
                            if matches!(op, FailRecovery) {
                                writer.execute_batch("CREATE TRIGGER fail_recovery BEFORE UPDATE ON tasks BEGIN SELECT RAISE(ABORT, 'injected recovery failure'); END;").unwrap();
                            }
                            let result = store.recover_task_start(snapshot, &recovered);
                            writer.execute_batch("DROP TRIGGER IF EXISTS fail_recovery;").unwrap();
                            if matches!(op, FailRecovery) && accepts {
                                prop_assert!(result.is_err());
                                prop_assert_eq!(store.get_task(task.id).unwrap().unwrap(), current);
                            } else {
                                prop_assert_eq!(result.unwrap(), accepts);
                                let after = store.get_task(task.id).unwrap().unwrap();
                                if accepts {
                                    version += 1;
                                    prop_assert_eq!(after.state, TaskState::Crashed);
                                    prop_assert_eq!(after.attempts, snapshot.attempts);
                                    prop_assert_eq!(after.execution_project, snapshot.execution_project.clone());
                                    prop_assert_eq!(after.execution_repo, snapshot.execution_repo.clone());
                                    prop_assert!(effects.iter().all(|e| matches!(e, Effect::Log { .. })), "recovery effects only log");
                                } else {
                                    prop_assert_eq!(after, current);
                                }
                            }
                        }
                    }
                }
                Restart => {
                    drop(store);
                    store = crate::store::Store::open(&path).unwrap();
                    prop_assert_eq!(store.get_task(task.id).unwrap().unwrap(), current);
                }
                Complete if current.state != TaskState::Completed => {
                    let mut changed = current;
                    // A later valid launch and completion both occur before
                    // recovery tries to commit its old starting observation.
                    changed.state = TaskState::Running;
                    commands::on_complete(&mut changed, Some("concurrent completion"), origin()).unwrap();
                    store.update_task(&changed).unwrap();
                    version += 1;
                }
                NewAttempt => {
                    let mut changed = current;
                    changed.state = TaskState::Starting;
                    changed.attempts += 1;
                    store.update_task(&changed).unwrap();
                    version += 1;
                }
                ChangeProject => {
                    let mut changed = current;
                    changed.execution_project = if changed.execution_project == "api" { "web" } else { "api" }.into();
                    changed.execution_repo = Some(format!("/repos/{}", changed.execution_project));
                    store.update_task(&changed).unwrap();
                    version += 1;
                }
                RecordSession if !live => {
                    session.attempt = current.attempts;
                    session.state = SessionState::Launching;
                    if inserted { store.update_session(&session).unwrap(); }
                    else { store.insert_session(&session).unwrap(); }
                    inserted = true;
                    live = true;
                }
                EndSession if live => {
                    session.state = SessionState::Exited;
                    store.update_session(&session).unwrap();
                    live = false;
                }
                _ => {}
            }
        }
    }
}

/// Keep the default gate small while allowing explicit stress runs.
fn model_config(cases: u32) -> ProptestConfig {
    ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(cases),
        ..ProptestConfig::default()
    }
}

fn model_steps() -> usize {
    std::env::var("POWERQUEUE_MODEL_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(40).max(2)
}

/// One call into the core, as the daemon or the CLI would make it.
#[derive(Debug, Clone)]
enum Op {
    Admission(crate::domain::GitHubAdmission),
    SourceAdmission(String, crate::domain::GitHubAdmission),
    HoldApproval,
    Restart,
    Pause,
    Resume,
    Cancel,
    Retry,
    SetModel(Option<ModelTier>),
    SetProject(String),
    Defer(Option<crate::deferral::Deferral>),
    SourceDeferral(String),
    CheckBinding {
        changed: bool,
    },
    /// The policy found no model (`Decision.model == None`).
    Throttle(Decision),
    /// The policy chose a model: `on_starting`.
    Start(ModelTier, Decision),
    Claim,
    /// Worktree or launcher failure after the claim: `on_crash` without a session.
    LaunchFailed(String),
    Launched {
        answered: bool,
    },
    Hook(HookOutcome),
    Probe {
        probe: SessionProbe,
        nudged: bool,
        pane_tail: Option<String>,
        cooldown_secs: Option<i64>,
    },
    /// `powerqueue task complete` (no PR): a direct write.
    Complete(Option<String>),
    /// `powerqueue task complete --pr <url>`: a direct write.
    HandOff(String),
    /// `powerqueue task block`: a direct write.
    Block(Option<String>),
    /// One poll of the PR (its number is patched to the task's PR).
    PrStatus(PrStatus),
    /// The Linear sync changed the task's links.
    Links {
        blocked_by: Vec<LinkedIssue>,
        children: Vec<LinkedIssue>,
    },
    /// A real parsed policy evaluation, including its model/reason trail.
    PolicyEvaluation(Evaluation),
    /// priority.toml re-scored the task.
    Evaluate {
        criticality: Criticality,
        score: f64,
        skip: bool,
    },
    AnswerQueued,
    ContainerClosed,
    Tick(i64),
}

/// The steps of a launch, as `launch_tasks` makes them for one candidate.
fn launch_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        2 => decision().prop_map(|mut d| {
            d.model = None;
            Op::Throttle(d)
        }),
        6 => (claude_model(), decision()).prop_map(|(m, mut d)| {
            d.model = Some(m.clone());
            d.retry_at = None;
            Op::Start(m, d)
        }),
        6 => Just(Op::Claim),
        1 => line().prop_map(Op::LaunchFailed),
        6 => any::<bool>().prop_map(|answered| Op::Launched { answered }),
    ]
}

/// Everything the daemon and the CLI do to one task outside a launch.
fn task_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => admission_decision().prop_map(Op::Admission),
        5 => source_routing_decision().prop_map(|(project, admission)| Op::SourceAdmission(project, admission)),
        2 => Just(Op::HoldApproval),
        1 => Just(Op::Restart),
        5 => prop::option::of(deferral()).prop_map(Op::Defer),
        4 => deferral_description().prop_map(Op::SourceDeferral),
        2 => Just(Op::Pause),
        3 => Just(Op::Resume),
        1 => Just(Op::Cancel),
        3 => Just(Op::Retry),
        1 => prop::option::of(claude_model()).prop_map(Op::SetModel),
        4 => execution_project().prop_map(Op::SetProject),
        2 => any::<bool>().prop_map(|changed| Op::CheckBinding { changed }),
        8 => hook_outcome().prop_map(Op::Hook),
        6 => (session_probe(), any::<bool>(), pane_tail(), prop::option::of(60i64..7200))
            .prop_map(|(probe, nudged, pane_tail, cooldown_secs)| Op::Probe { probe, nudged, pane_tail, cooldown_secs }),
        2 => prop::option::of(line()).prop_map(Op::Complete),
        3 => any_pr_url().prop_map(Op::HandOff),
        1 => prop::option::of(line()).prop_map(Op::Block),
        4 => pr_status(1).prop_map(Op::PrStatus),
        1 => (vec(linked_issue(), 0..3), vec(linked_issue(), 0..2))
            .prop_map(|(blocked_by, children)| Op::Links { blocked_by, children }),
        3 => (criticality(), 0u32..2000, prop::bool::weighted(0.2))
            .prop_map(|(criticality, score, skip)| Op::Evaluate { criticality, score: score as f64, skip }),
        2 => Just(Op::AnswerQueued),
        1 => Just(Op::ContainerClosed),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        21 => launch_op(),
        41 => task_op(),
        4 => (1i64..3 * 3600).prop_map(Op::Tick),
    ]
}

/// Everything the daemon holds for one task.
#[derive(Debug, Clone)]
struct TaskWorld {
    task: Task,
    /// The latest session row, live or not.
    session: Option<Session>,
    /// A start under way (`on_starting` done, launch not finished).
    start: Option<launch::Start>,
    /// `claim` ran for `start`.
    claimed: bool,
}

impl TaskWorld {
    fn new(task: Task) -> Self {
        Self { task, session: None, start: None, claimed: false }
    }

    fn session_live(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.state.is_live())
    }

    /// What the planner hands out: schedulable, waiting on nothing, retry time passed, no live session.
    fn can_start(&self, now: DateTime<Utc>) -> bool {
        self.task.state.is_schedulable()
            && self.task.execution_approved()
            && !self.task.is_waiting()
            && self.task.not_before.is_none_or(|t| t <= now)
            && self.task.deferral.as_ref().is_none_or(|d| d.until <= now)
            && self.task.source_deferral().is_ok_and(|d| d.is_none_or(|d| d.until <= now))
            && !self.session_live()
            && self.start.is_none()
    }

    /// Run one op; `None` when its precondition does not hold (skipped).
    /// `Tick` is the world's business and is never applied here.
    fn apply(&mut self, cfg: &Config, now: DateTime<Utc>, op: &Op) -> Option<Vec<Effect>> {
        let live = self.session_live();
        // This general-purpose world treats preparation as one atomic launch.
        // The focused background_setup world below explicitly interleaves
        // commands and passes with pending workers across tick boundaries.
        // Direct CLI writes must still refuse while starting (checked here).
        let direct = matches!(
            op,
            Op::Complete(_) | Op::HandOff(_) | Op::Block(_) | Op::Defer(_) | Op::SetProject(_) | Op::CheckBinding { .. }
        );
        if self.start.is_some()
            && !direct
            && !matches!(
                op,
                Op::Claim
                    | Op::LaunchFailed(_)
                    | Op::Launched { .. }
                    | Op::Admission(_)
                    | Op::SourceAdmission(..)
                    | Op::HoldApproval
            )
        {
            return None;
        }
        match op {
            Op::Admission(admission) => matches!(self.task.source, TaskSource::GitHub { .. })
                .then(|| transitions::on_github_admission(&mut self.task, admission.clone())),
            Op::SourceAdmission(project, admission) => matches!(self.task.source, TaskSource::GitHub { .. })
                .then(|| transitions::on_github_source_admission(&mut self.task, admission.clone(), project)),
            Op::HoldApproval => {
                if self.start.is_none() || self.task.execution_approved() {
                    return None;
                }
                let effects = transitions::on_github_launch_held(&mut self.task);
                self.start = None;
                self.claimed = false;
                Some(effects)
            }
            Op::Restart => {
                self.task = serde_json::from_value(serde_json::to_value(&self.task).unwrap()).unwrap();
                self.session = serde_json::from_value(serde_json::to_value(&self.session).unwrap()).unwrap();
                Some(Vec::new())
            }
            Op::Defer(deferral) => Some(commands::on_defer(&mut self.task, deferral.clone()).unwrap_or_default()),
            Op::SourceDeferral(description) => {
                let before = self.task.clone();
                self.task.description = description.clone();
                Some(commands::source_deferral_events(&before, &self.task))
            }
            Op::Pause => Some(commands::on_pause(&mut self.task, live)),
            Op::Resume => Some(commands::on_resume(&mut self.task, live, now)),
            Op::Cancel => Some(commands::on_cancel(&mut self.task, self.session.as_mut(), now)),
            Op::Retry => Some(commands::on_retry(&mut self.task, self.session.as_mut(), now)),
            Op::SetModel(m) => Some(commands::on_set_model(&mut self.task, m.clone())),
            Op::SetProject(project) => {
                Some(commands::on_set_project(&mut self.task, project, self.session.is_some()).unwrap_or_default())
            }
            Op::CheckBinding { changed } => {
                let bound = self.task.execution_repo.clone()?;
                let checkout = if *changed { format!("{bound}/moved") } else { bound };
                Some(commands::bind_project(&mut self.task, &checkout).unwrap_or_default())
            }
            Op::Throttle(d) => self.can_start(now).then(|| launch::on_throttled(&mut self.task, d, now)),
            Op::Start(model, d) => {
                if !self.can_start(now) {
                    return None;
                }
                let resuming = self.task.review_relaunch().is_some();
                let checkout = if cfg.repo.path.is_empty() {
                    format!("/repos/{}", self.task.execution_project)
                } else {
                    cfg.repo.path.clone()
                };
                let mut effects = commands::bind_project(&mut self.task, &checkout).expect("same checkout");
                let root = std::path::PathBuf::from("/wt").join(&self.task.execution_project);
                let start = launch::on_starting(&mut self.task, model, d, cfg, &root, resuming).expect("model task is approved");
                effects.extend(start.effects.clone());
                self.start = Some(start);
                self.claimed = false;
                Some(effects)
            }
            Op::Claim => {
                let start = self.start.as_ref()?;
                if self.claimed {
                    return None;
                }
                launch::claim(&mut self.task, start);
                self.claimed = true;
                Some(Vec::new())
            }
            Op::LaunchFailed(reason) => {
                if self.start.is_none() || !self.claimed {
                    return None;
                }
                self.start = None;
                self.claimed = false;
                Some(transitions::on_crash(&mut self.task, None, reason, None, &cfg.scheduler, now, None))
            }
            Op::Launched { answered } => {
                let start = self.start.take()?;
                if !self.claimed || !self.task.execution_approved() {
                    self.start = Some(start);
                    return None;
                }
                self.claimed = false;
                let model = self.task.model.clone().unwrap_or_default();
                let review = self.task.review_relaunch().is_some();
                let (id, resume, agent_session_id) = launch::resume_plan(self.session.as_ref(), &model, true, review);
                let session = Session {
                    id,
                    task_id: self.task.id,
                    attempt: start.attempt,
                    model: model.clone(),
                    state: SessionState::Launching,
                    tmux_session: "powerqueue".into(),
                    tmux_window: "@1".into(),
                    pane_id: Some("%1".into()),
                    pid: Some(42),
                    transcript_path: None,
                    exit_code: None,
                    started_at: now,
                    ended_at: None,
                    last_activity_at: now,
                    error: None,
                    agent_session_id,
                    waiting_since: None,
                    waited_secs: 0,
                };
                let relaunch = self.task.review_relaunch().cloned();
                let review_prompt = relaunch.as_ref().map(|r| format!("/ship-pr {} --reason {}", r.pr_number, r.reason));
                let facts = launch::Launched {
                    model: &model,
                    attempt: start.attempt,
                    resume,
                    relaunch: relaunch.as_ref(),
                    review_prompt: review_prompt.as_deref(),
                    resume_prompt: review_prompt.as_deref(),
                    branch: &start.branch,
                    worktree: &start.worktree,
                    answered: *answered,
                };
                let effects = launch::on_launched(&mut self.task, &session, &facts, cfg, now);
                self.session = Some(session);
                Some(effects)
            }
            Op::Hook(outcome) => {
                let session = self.session.as_mut()?;
                Some(transitions::on_hook_outcome(&mut self.task, session, outcome, cfg, now, now + Duration::days(3)))
            }
            Op::Probe { probe, nudged, pane_tail, cooldown_secs } => {
                if !live {
                    return None;
                }
                let cooldown = self.provider_cooldown(cfg, now, pane_tail.as_deref(), *cooldown_secs);
                let session = self.session.as_mut()?;
                let ctx = ProbeContext { now, nudged: *nudged, pane_tail: pane_tail.clone(), provider_cooldown_until: cooldown };
                Some(transitions::on_probe(&mut self.task, session, probe, &cfg.scheduler, &ctx))
            }
            Op::Complete(summary) => {
                // `task complete` as the CLI writes it, then the daemon's finalize.
                let mut effects = commands::on_complete(&mut self.task, summary.as_deref(), now).ok()?;
                effects.extend(self.finalize(now));
                Some(effects)
            }
            Op::HandOff(url) => {
                // `task complete --pr` as the CLI writes it, then the daemon's finalize.
                let mut effects = commands::on_hand_off(&mut self.task, url, None, now).ok()?;
                effects.extend(self.finalize(now));
                Some(effects)
            }
            Op::Block(reason) => commands::on_block(&mut self.task, reason.as_deref()).ok(),
            Op::PrStatus(status) => {
                if self.task.state != TaskState::InReview || live {
                    return None;
                }
                let number = self.task.pr_url.as_deref().and_then(review::pr_number_of)?;
                let status = PrStatus { number, ..status.clone() };
                Some(review::on_pr_status(&mut self.task, &status, &cfg.scheduler, now))
            }
            Op::Links { blocked_by, children } => {
                self.task.blocked_by = blocked_by.clone();
                self.task.children = children.clone();
                Some(Vec::new())
            }
            Op::PolicyEvaluation(eval) => {
                if self.task.state.has_live_session() || live {
                    return None;
                }
                Some(transitions::on_evaluation(&mut self.task, eval))
            }
            Op::Evaluate { criticality, score, skip } => {
                // The daemon never re-scores under a live session (a
                // throttled or paused task may keep one).
                if self.task.state.has_live_session() || live {
                    return None;
                }
                let eval = Evaluation {
                    criticality: *criticality,
                    score: *score,
                    model: None,
                    models: Vec::new(),
                    model_source: None,
                    skip: *skip,
                    reasons: Vec::new(),
                };
                Some(transitions::on_evaluation(&mut self.task, &eval))
            }
            Op::AnswerQueued => (!live).then(|| transitions::on_answer_queued(&mut self.task, &["c1".into()], now)),
            Op::ContainerClosed => {
                if !self.task.is_container() || self.task.state.is_terminal() || live {
                    return None;
                }
                let children: Vec<LinkedIssue> =
                    self.task.children.iter().map(|c| LinkedIssue { state_type: "completed".into(), ..c.clone() }).collect();
                Some(transitions::on_container_closed(&mut self.task, &children, now))
            }
            Op::Tick(_) => None,
        }
    }

    /// What `probe_sessions` passes as `provider_cooldown_until`: only for
    /// a session that went stale or ran out of time whose pane shows the
    /// agent waiting for a usage reset, the cooldown the daemon starts
    /// (`cooldown_secs` from now, at least a minute). `None` otherwise.
    fn provider_cooldown(
        &self,
        cfg: &Config,
        now: DateTime<Utc>,
        pane_tail: Option<&str>,
        cooldown_secs: Option<i64>,
    ) -> Option<DateTime<Utc>> {
        let session = self.session.as_ref()?;
        let sc = &cfg.scheduler;
        let stale = matches!(self.task.state, TaskState::Running | TaskState::Starting)
            && sc.stale_session_secs > 0
            && now - session.last_activity_at > Duration::seconds(sc.stale_session_secs as i64);
        let timeout = transitions::session_timed_out(&self.task, session, sc, now);
        if !(stale || timeout) || !pane_tail.is_some_and(transitions::pane_waits_for_usage_reset) {
            return None;
        }
        cooldown_secs.map(|secs| now + Duration::seconds(secs.max(60)))
    }

    /// The daemon's finalize phase: a live session of a task that was
    /// handed off or finished outside the hook path is released.
    fn finalize(&mut self, now: DateTime<Utc>) -> Vec<Effect> {
        match self.session.as_mut() {
            Some(s) if s.state.is_live() => transitions::release_session(&self.task, s, now),
            _ => Vec::new(),
        }
    }
}

/// A fresh task as `powerqueue add` or the sync would insert it.
fn fresh_task() -> impl Strategy<Value = Task> {
    (word(), criticality(), 0.0f64..2000.0, prop::option::of(1u32..=4), vec(word(), 0..2), execution_project())
        .prop_map(|(key, criticality, score, max_attempts, labels, project)| {
            let mut t = Task::new(format!("ENG-{key}"), "t", TaskSource::Manual);
            t.execution_project = project;
            t.criticality = criticality;
            t.score = score;
            t.max_attempts = max_attempts;
            t.labels = labels;
            t.created_at = origin() - Duration::hours(1);
            t.updated_at = t.created_at;
            t
        })
        .prop_flat_map(with_source_admission)
}

fn check(
    step: usize,
    op: &Op,
    cfg: &Config,
    before: &TaskWorld,
    after: &TaskWorld,
    effects: &[Effect],
) -> Result<(), TestCaseError> {
    let (b, a) = (&before.task, &after.task);
    if let Op::SetProject(project) = op {
        let eligible = before.session.is_none()
            && b.execution_repo.is_none()
            && b.attempts == 0
            && b.started_at.is_none()
            && b.branch.is_none()
            && b.worktree_path.is_none()
            && b.review.is_none()
            && b.pr_url.is_none()
            && matches!(
                b.state,
                TaskState::Queued | TaskState::Paused | TaskState::Blocked | TaskState::Throttled | TaskState::NeedsAttention
            );
        let mut expected = b.clone();
        if eligible {
            expected.execution_project = project.clone();
            expected.project_assignment = Some(crate::routing::ProjectAssignment::Explicit { project: project.clone() });
            if b.project_assignment != expected.project_assignment || b.execution_project != *project {
                expected.routing = None;
            }
        }
        prop_assert_eq!(a, &expected, "reassignment changed history or bypassed its guard");
        prop_assert_eq!(&before.session, &after.session);
        prop_assert_eq!(effects.is_empty(), !eligible || a == b);
    } else {
        prop_assert_eq!(&a.execution_project, &b.execution_project, "execution project changed in a lifecycle transition");
    }
    if matches!(op, Op::CheckBinding { .. }) {
        prop_assert_eq!(a, b, "checking or rejecting a checkout changed the task");
        prop_assert_eq!(&before.session, &after.session);
        prop_assert!(effects.is_empty());
    }
    if let Some(bound) = &b.execution_repo {
        prop_assert_eq!(a.execution_repo.as_ref(), Some(bound), "checkout changed on retry/review");
    }
    if let Some(path) = &a.worktree_path {
        prop_assert!(
            std::path::Path::new(path).starts_with(std::path::Path::new("/wt").join(&a.execution_project)),
            "worktree crossed projects"
        );
    }
    let ctx = |what: &str| {
        format!(
            "step {step} {op:?}: {what}\n  before: {:?} {:?}\n  after:  {:?} {:?}",
            b.state, b.not_before, a.state, a.not_before
        )
    };

    if let Op::Defer(value) = op {
        let mut expected = b.clone();
        if b.state != TaskState::Starting {
            expected.deferral = value.clone();
        }
        prop_assert_eq!(a, &expected, "deferral altered execution state or bypassed a launch claim");
        prop_assert_eq!(&before.session, &after.session, "deferral terminated active work");
        prop_assert!(effects.iter().all(|e| matches!(e, Effect::Log { .. })), "deferral only logs");
    } else {
        prop_assert_eq!(&a.deferral, &b.deferral, "automatic lifecycle operation erased a user hold");
    }

    // Legal moves. Closing a container (a parent issue whose sub-issues are
    // done) is the one move the table does not list: it happens from any
    // open state and must not open `task complete` to queued tasks.
    if !matches!(op, Op::ContainerClosed) {
        prop_assert!(b.state.can_transition_to(a.state), "{}", ctx("move not in TaskState::can_transition_to"));
    }

    // A launch under way is never interrupted: `start_task` persists
    // `running` when it ends, so a direct write landing in between would
    // be lost. Only the launch's own steps change it; project commands refuse
    // or confirm the existing binding without modifying it.
    if before.start.is_some() {
        prop_assert!(
            matches!(
                op,
                Op::Claim
                    | Op::LaunchFailed(_)
                    | Op::Launched { .. }
                    | Op::SetProject(_)
                    | Op::Defer(_)
                    | Op::CheckBinding { .. }
                    | Op::Admission(_)
                    | Op::SourceAdmission(..)
                    | Op::HoldApproval
            ),
            "{}",
            ctx("applied to a task the daemon is launching")
        );
    }

    if matches!(op, Op::Start(..) | Op::Launched { .. }) {
        prop_assert!(b.execution_approved(), "started or launched without approval");
    }
    if let Op::SourceAdmission(project, admission) = op {
        prop_assert_eq!(
            a.execution_approved(),
            admission.approved && project == &b.execution_project,
            "source routing bypassed approval or accepted a conflicting destination"
        );
        let mut expected = b.clone();
        expected.github_admission = a.github_admission.clone();
        prop_assert_eq!(a, &expected, "source sync retargeted or rewrote execution history");
        prop_assert!(effects.iter().all(|e| matches!(e, Effect::Log { .. })), "routing must only log");
    }
    if matches!(op, Op::Admission(_) | Op::SourceAdmission(..)) {
        prop_assert_eq!(&before.session, &after.session, "revocation destroyed active work");
        prop_assert_eq!(b.state, a.state, "admission changed execution state");
        prop_assert_eq!(&b.worktree_path, &a.worktree_path);
    }
    if matches!(op, Op::Restart) {
        prop_assert_eq!(b, a, "restart lost durable admission or lifecycle state");
        prop_assert_eq!(&before.session, &after.session);
    }

    // Session / task agreement.
    if let Some(s) = &after.session {
        prop_assert_eq!(s.task_id, a.id);
        if s.state.is_live() {
            prop_assert!(a.state.may_keep_session(), "{}", ctx("live session in a state that has none"));
            prop_assert_eq!(s.attempt, a.attempts, "{}", ctx("live session attempt differs from the task's"));
        }
    }
    if matches!(a.state, TaskState::Running | TaskState::Idle) {
        prop_assert!(after.session_live(), "{}", ctx("running/idle without a live session"));
    }

    // Backoff and retry times only while crashed or throttled.
    if a.not_before.is_some() {
        prop_assert!(matches!(a.state, TaskState::Crashed | TaskState::Throttled), "{}", ctx("not_before set"));
    }

    // Attempts move only through claim and a retry from a terminal state.
    match op {
        Op::Claim if a.attempts != b.attempts => prop_assert_eq!(a.attempts, b.attempts + 1, "{}", ctx("claim")),
        Op::Retry if b.state.is_terminal() && a.state == TaskState::Queued => {
            prop_assert_eq!(a.attempts, 0, "{}", ctx("retry of a finished task"));
            prop_assert!(a.last_error.is_none() && a.summary.is_none() && a.completed_at.is_none() && a.started_at.is_none());
        }
        Op::Claim | Op::Retry => {}
        _ => prop_assert_eq!(a.attempts, b.attempts, "{}", ctx("attempts changed")),
    }
    if a.started_at.is_some() {
        prop_assert!(a.attempts >= 1, "{}", ctx("started without an attempt"));
    }

    // Terminal states are only left by a retry; the one move between them
    // is failed → completed: the done marker of the last attempt drained
    // after the probe gave up on it, or a human who finished the work.
    if b.state.is_terminal() && !a.state.is_terminal() {
        prop_assert!(matches!(op, Op::Retry), "{}", ctx("left a terminal state"));
    }
    if b.state.is_terminal() && a.state.is_terminal() && a.state != b.state {
        prop_assert!(
            (b.state, a.state) == (TaskState::Failed, TaskState::Completed)
                && matches!(op, Op::Hook(HookOutcome::Completed { .. }) | Op::Complete(_)),
            "{}",
            ctx("moved between terminal states")
        );
    }
    prop_assert_eq!(a.completed_at.is_some(), a.state.is_terminal(), "{}", ctx("completed_at vs terminal"));

    // Review bookkeeping.
    if a.state == TaskState::InReview {
        prop_assert!(a.pr_url.as_deref().and_then(review::pr_number_of).is_some(), "{}", ctx("in review without a PR"));
        let w = a.review.as_ref();
        prop_assert!(w.is_some(), "{}", ctx("in review without a watch"));
        prop_assert!(!w.is_some_and(|w| w.parked), "{}", ctx("in review while parked"));
    }
    if let Some(w) = &a.review {
        prop_assert!(w.rounds <= cfg.scheduler.review_rounds_max.max(b.review.as_ref().map_or(0, |r| r.rounds)));
        prop_assert!(w.rounds <= b.review.as_ref().map_or(0, |r| r.rounds) + 1, "{}", ctx("rounds jumped"));
    }

    // After a re-score, the dependency state matches the links.
    if matches!(op, Op::Evaluate { .. } | Op::PolicyEvaluation(_)) {
        if a.state == TaskState::Blocked {
            prop_assert!(a.is_waiting(), "{}", ctx("blocked while waiting on nothing"));
        }
        if matches!(a.state, TaskState::Queued | TaskState::Throttled) {
            prop_assert!(!a.is_waiting(), "{}", ctx("schedulable while waiting"));
        }
    }

    // Effects: well-formed, justified, and a replayable trace.
    for e in effects {
        let json = serde_json::to_string(e).expect("effects serialise");
        let back: Effect = serde_json::from_str(&json).expect("effects deserialise");
        prop_assert_eq!(&back, e);
        match e {
            Effect::Log { kind, message, .. } => {
                prop_assert!(!kind.is_empty() && !message.is_empty(), "{}", ctx("empty log"));
            }
            Effect::KillWindow => {
                prop_assert!(before.session_live(), "{}", ctx("kill window without a live session"));
            }
            Effect::Cleanup { .. } => {
                prop_assert!(a.state.is_terminal(), "{}", ctx("cleanup of a task that is not finished"));
            }
            Effect::ReleaseForReview => {
                prop_assert_eq!(a.state, TaskState::InReview, "{}", ctx("release for review"));
            }
            Effect::DeleteBranch => {
                prop_assert_eq!(a.state, TaskState::Completed, "{}", ctx("branch deleted"));
            }
            _ => {}
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(model_config(256))]

    /// Any sequence of daemon calls keeps the task, its session and the
    /// effect trace consistent (see the module doc for the invariants).
    #[test]
    fn a_task_survives_any_sequence_of_daemon_calls(task in fresh_task(), cfg in scheduler_config(), ops in vec(op(), 1..model_steps())) {
        let cfg = Config { scheduler: cfg, ..Config::default() };
        let mut now = origin();
        let mut world = TaskWorld::new(task);
        for (step, op) in ops.iter().enumerate() {
            if let Op::Tick(secs) = op {
                now += Duration::seconds(*secs);
                continue;
            }
            let before = world.clone();
            let Some(effects) = world.apply(&cfg, now, op) else { continue };
            check(step, op, &cfg, &before, &world, &effects)?;
        }
    }
}

proptest! {
    /// Force meaningful routing interleavings as well as the arbitrary walks:
    /// denied setup, retry/restart, successful launch, active revocation and review.
    #[test]
    fn source_routing_interleaves_with_setup_retry_restart_and_review(
        project in execution_project(), mut decision in decision()
    ) {
        let cfg = Config::default();
        let now = origin();
        let mut task = Task::new("acme/app#1", "routed task", TaskSource::GitHub {
            repository: "acme/app".into(), number: 1, url: "https://github.com/acme/app/issues/1".into()
        });
        task.execution_project = project.clone();
        let other = if project == "default" { "api" } else { "default" };
        let verified = crate::domain::GitHubAdmission { approved: true, reason: "verified".into() };
        let admit = Op::SourceAdmission(project.clone(), verified.clone());
        let conflict = Op::SourceAdmission(other.into(), verified);
        decision.model = Some(ModelTier::opus());
        decision.retry_at = None;
        let start = Op::Start(ModelTier::opus(), decision);
        let launch = Op::Launched { answered: false };
        let mut world = TaskWorld::new(task);
        let schedule = [
            (admit.clone(), true), (start.clone(), true), (Op::Claim, true),
            (conflict.clone(), true), (launch.clone(), false), (Op::HoldApproval, true),
            (Op::Restart, true), (Op::Retry, true), (start.clone(), false),
            (admit.clone(), true), (start.clone(), true), (Op::Claim, true), (launch.clone(), true),
            (conflict, true), (Op::Restart, true),
            (Op::HandOff("https://github.com/acme/app/pull/1".into()), true),
            (Op::Restart, true), (Op::Retry, true), (start.clone(), false),
            (admit, true), (start, true), (Op::Claim, true), (launch, true),
        ];
        for (step, (op, accepted)) in schedule.iter().enumerate() {
            let before = world.clone();
            let effects = world.apply(&cfg, now, op);
            prop_assert_eq!(effects.is_some(), *accepted, "step {} {:?}", step, op);
            if let Some(effects) = effects {
                check(step, op, &cfg, &before, &world, &effects)?;
            }
            prop_assert_eq!(&world.task.execution_project, &project);
        }
        prop_assert!(world.session_live());
        prop_assert_eq!(world.task.state, TaskState::Running);
        prop_assert_eq!(world.task.execution_repo, Some(format!("/repos/{project}")));
    }
}

// ------------------------------------------------------------ whole daemon

/// How a start that the planner handed out ends in `start_task`.
#[derive(Debug, Clone)]
enum LaunchOutcome {
    ClaimLost,
    ApprovalLost,
    RouteChanged,
    /// The session launched (`on_launched`).
    Launched {
        answered: bool,
    },
    /// The worktree or the launcher failed first (`on_crash` without a session).
    Failed(String, super::launch_failures::FailureScope),
}

fn launch_outcome() -> impl Strategy<Value = LaunchOutcome> {
    prop_oneof![
        3 => any::<bool>().prop_map(|answered| LaunchOutcome::Launched { answered }),
        2 => (line(), crate::strategies::launch_failure_scope()).prop_map(|(reason, scope)| LaunchOutcome::Failed(reason, scope)),
        1 => Just(LaunchOutcome::ApprovalLost),
        1 => Just(LaunchOutcome::ClaimLost),
        1 => Just(LaunchOutcome::RouteChanged),
    ]
}

/// One thing that happens to the daemon between two launch passes, or a pass.
#[derive(Debug, Clone)]
enum DaemonOp {
    /// A per-task call from [`task_op`] (never a launch step or a tick:
    /// those are the pass's and the world's), the index taken modulo the
    /// number of tasks.
    Task(usize, Op),
    /// `launch_tasks`: the outcomes are consumed by the starts in order
    /// (a launch, when the list runs out).
    LaunchPass(Vec<LaunchOutcome>),
    /// Dated transcript usage reached the store, possibly after the session ended.
    SpendRecorded {
        task: usize,
        tokens: u64,
        age_secs: i64,
    },
    Observation(crate::domain::Provider, f64, bool),
    ReloadConcurrency(u32),
    ReloadPolicy(String, String),
    SpecialLabel(usize, bool),
    ProjectSettings(String, (bool, bool, ModelTier)),
    PauseScheduling,
    ResumeScheduling,
    Tick(i64),
}

fn daemon_op() -> impl Strategy<Value = DaemonOp> {
    prop_oneof![
        40 => (0usize..4, task_op()).prop_map(|(i, op)| DaemonOp::Task(i, op)),
        20 => vec(launch_outcome(), 0..4).prop_map(DaemonOp::LaunchPass),
        4 => (0usize..4, 1u64..2_000_000, 0i64..6 * 3600).prop_map(|(task, tokens, age_secs)| DaemonOp::SpendRecorded { task, tokens, age_secs }),
        3 => provider_reading().prop_map(|(p, fraction, reset)| DaemonOp::Observation(p, fraction, reset)),
        3 => (1u32..=4).prop_map(DaemonOp::ReloadConcurrency),
        3 => (execution_project(), conditional_model_policy()).prop_map(|(p, rules)| DaemonOp::ReloadPolicy(p, rules)),
        3 => (0usize..4, any::<bool>()).prop_map(|(i, enabled)| DaemonOp::SpecialLabel(i, enabled)),
        5 => (execution_project(), execution_project_settings()).prop_map(|(id, settings)| DaemonOp::ProjectSettings(id, settings)),
        1 => Just(DaemonOp::PauseScheduling),
        2 => Just(DaemonOp::ResumeScheduling),
        6 => budget_tick().prop_map(DaemonOp::Tick),
    ]
}

/// What `launch_tasks` did in one pass.
#[derive(Debug, Clone, PartialEq, Default)]
struct Pass {
    /// Each candidate handed out, in order, with the policy's verdict.
    handed: Vec<(TaskId, Option<ModelTier>, Option<DateTime<Utc>>)>,
    /// Sessions that launched.
    starts: u32,
    /// Slots the pass had.
    slots: u32,
}

/// Everything the daemon holds across its tasks.
#[derive(Clone)]
struct DaemonWorld {
    cfg: Config,
    rules: HashMap<String, PriorityRules>,
    checkouts: HashMap<String, String>,
    now: DateTime<Utc>,
    tasks: Vec<TaskWorld>,
    /// The stored usage (what `Ledgers::load` returns at the next pass).
    ledgers: Ledgers,
    budget_store: crate::store::Store,
    rate_limits: RateLimitState,
    estimator: Estimator,
    scheduling_paused: bool,
    launch_failures: super::launch_failures::LaunchFailures,
}

// Store clones share only persisted budget input. Reversed launch passes read
// that input and reserve on their own ledger copies; they never write the store.
impl std::fmt::Debug for DaemonWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonWorld")
            .field("cfg", &self.cfg)
            .field("tasks", &self.tasks)
            .field("now", &self.now)
            .field("ledgers", &self.ledgers)
            .finish_non_exhaustive()
    }
}

impl DaemonWorld {
    fn set_project(&mut self, id: &str, (available, moved, model): &(bool, bool, ModelTier)) {
        self.checkouts.insert(id.into(), format!("/{}/{id}", if *moved { "moved" } else { "repos" }));
        if *available {
            let rules = PriorityRules::parse(&format!(
                "version = 1\n[models]\nnormal = [\"{}\"]\ncritical = [\"{}\"]\n",
                model.alias(),
                model.alias()
            ))
            .expect("generated model rules");
            self.rules.insert(id.into(), rules);
        } else {
            self.rules.remove(id);
        }
    }

    fn live_count(&self) -> u32 {
        self.tasks.iter().filter(|t| t.session_live()).count() as u32
    }

    // Independent capacity oracle: count actual live session rows rather than
    // consulting QueueSlots or the planner's remaining-slot bookkeeping.
    fn queue_room(&self, task: &Task) -> bool {
        let queue = task.execution.as_ref().map_or("default", |e| e.queue.as_str());
        if self.cfg.queues.is_empty() {
            return queue == "default";
        }
        let live = self
            .tasks
            .iter()
            .filter(|t| t.session_live() && t.task.execution.as_ref().map_or("default", |e| e.queue.as_str()) == queue)
            .count() as u32;
        self.cfg.queues.get(queue).is_some_and(|q| live < q.max_concurrent)
    }

    fn ready(&self) -> Vec<TaskId> {
        self.tasks
            .iter()
            .filter(|t| t.can_start(self.now) && self.rules.contains_key(&t.task.execution_project))
            .map(|t| t.task.id)
            .collect()
    }

    /// Whether two ready tasks are indistinguishable to `pick_next` (same
    /// score, criticality and age): then the store's row order decides.
    fn has_ties(&self) -> bool {
        let mut keys = HashSet::new();
        self.tasks
            .iter()
            .filter(|t| t.can_start(self.now))
            .any(|t| !keys.insert((t.task.score.to_bits(), t.task.criticality, t.task.created_at)))
    }

    /// Use the real usage writer, SQL range aggregation, clock resolution and
    /// pure ledger builder. Pass reservations remain outside this stored history.
    fn refresh_ledgers(&mut self) {
        self.ledgers = Ledgers::load(&self.budget_store, &self.cfg.budget, self.now).unwrap();
    }

    /// The daemon carries out the effects that feed the next pass.
    fn carry_out(&mut self, effects: &[Effect]) {
        for e in effects {
            match e {
                Effect::RateLimit { tier, until } => self.rate_limits.mark(tier.clone(), *until),
                Effect::RateLimitProvider { provider, until } => {
                    self.rate_limits.mark_provider(&self.cfg.budget, *provider, *until)
                }
                _ => {}
            }
        }
    }

    /// One per-task call, checked. `Ok(None)` when its precondition did not hold.
    fn step(&mut self, step: usize, idx: usize, op: &Op) -> Result<Option<Vec<Effect>>, TestCaseError> {
        let now = self.now;
        let before = self.tasks[idx].clone();
        let mut cfg = self.cfg.clone();
        // Unresolved automatic routing has no checkout, but command/admission
        // transitions must still work before a repository is selected.
        cfg.repo.path = if before.task.execution_project.is_empty() {
            String::new()
        } else {
            self.checkouts[&before.task.execution_project].clone()
        };
        let Some(effects) = self.tasks[idx].apply(&cfg, now, op) else { return Ok(None) };
        check(step, op, &self.cfg, &before, &self.tasks[idx], &effects)?;
        self.carry_out(&effects);
        // A stale or timed-out pane waiting for a usage reset put the
        // provider on cooldown before the probe (`start_provider_cooldown`).
        if let Op::Probe { pane_tail, cooldown_secs, .. } = op
            && let Some(until) = before.provider_cooldown(&self.cfg, now, pane_tail.as_deref(), *cooldown_secs)
            && let Some(session) = before.session.as_ref()
        {
            self.rate_limits.mark_provider(&self.cfg.budget, session.model.provider(), until);
        }
        Ok(Some(effects))
    }

    /// `launch_tasks`, with the per-task steps and the pass invariants checked.
    fn launch_pass(&mut self, step: usize, outcomes: &[LaunchOutcome]) -> Result<Pass, TestCaseError> {
        let now = self.now;
        let mut pass = Pass::default();
        if self.scheduling_paused {
            return Ok(pass);
        }
        let slots = self.cfg.scheduler.max_concurrent.saturating_sub(self.live_count());
        pass.slots = slots;
        if slots == 0 {
            return Ok(pass);
        }
        let candidates: Vec<Task> =
            self.tasks.iter().filter(|t| !t.task.state.is_terminal() && !t.session_live()).map(|t| t.task.clone()).collect();
        if !candidates.iter().any(|t| t.state.is_schedulable()) {
            return Ok(pass);
        }
        self.refresh_ledgers();
        self.rate_limits.clear_expired(now);
        let ready = self.ready();
        let at_start = self.ledgers.clone();
        let margin = (1.0 - self.cfg.budget.safety_margin).max(0.0);
        let live: Vec<_> = self.tasks.iter().filter(|t| t.session_live()).map(|t| t.task.clone()).collect();
        let mut planner =
            LaunchPlanner::new(self.estimator.clone(), self.ledgers.clone(), candidates, slots).with_queues(&self.cfg, &live);
        let mut outcomes = outcomes.iter();
        loop {
            planner.set_failures(self.launch_failures.clone());
            let Some(candidate) = planner.next_for_projects(&self.cfg.budget, &self.rules, &self.rate_limits, now) else { break };
            prop_assert!(self.launch_failures.permits(&candidate.task.execution_project, now), "held project was considered");
            let idx = self.tasks.iter().position(|t| t.task.id == candidate.task.id).expect("a candidate is one of the tasks");
            prop_assert_eq!(
                &candidate.preferred,
                &launch::preferred_models(&self.rules[&candidate.task.execution_project], &candidate.task, now),
                "wrong project's policy"
            );
            let what = format!(
                "step {step} pass: {} ({:?}, score {}) decided {:?}",
                candidate.task.key, candidate.task.state, candidate.task.score, candidate.decision.model
            );
            prop_assert!(!pass.handed.iter().any(|(id, ..)| *id == candidate.task.id), "{what}: handed out twice");
            prop_assert!(candidate.task.deferral_allows(now), "launch before user eligibility");
            prop_assert!(ready.contains(&candidate.task.id), "{what}: not ready when the pass began");
            prop_assert!(self.queue_room(&candidate.task), "{what}: missing or saturated queue was considered");
            prop_assert!(pass.starts < slots, "{what}: handed out after the slots were used");
            pass.handed.push((candidate.task.id, candidate.decision.model.clone(), candidate.decision.retry_at));
            match candidate.decision.model.clone() {
                None => {
                    let applied = self.step(step, idx, &Op::Throttle(candidate.decision.clone()))?;
                    prop_assert!(applied.is_some(), "{what}: the throttle was refused");
                    let task = &self.tasks[idx].task;
                    let retry_at = candidate.decision.retry_at.unwrap_or(now + WINDOW_RECHECK);
                    prop_assert_eq!((task.state, task.not_before), (TaskState::Throttled, Some(retry_at)), "{}", what);
                }
                Some(model) => {
                    let task = &self.tasks[idx].task;
                    let checkout = &self.checkouts[&task.execution_project];
                    if task.execution_repo.as_ref().is_some_and(|bound| bound != checkout) {
                        // The shell rejects this before on_starting or any repository operation.
                        let mut rejected = task.clone();
                        prop_assert!(commands::bind_project(&mut rejected, checkout).is_err());
                        prop_assert_eq!(&rejected, task);
                        self.launch_failures.failed(
                            task,
                            super::launch_failures::FailureScope::Project,
                            "checkout binding changed".into(),
                            now,
                        );
                        continue;
                    }
                    let outcome = outcomes.next().cloned().unwrap_or(LaunchOutcome::Launched { answered: false });
                    if matches!(outcome, LaunchOutcome::ClaimLost) {
                        let attempts = self.tasks[idx].task.attempts;
                        let failures = self.launch_failures.clone();
                        self.step(step, idx, &Op::Pause)?;
                        prop_assert_eq!(self.tasks[idx].task.attempts, attempts);
                        prop_assert_eq!(&self.launch_failures, &failures);
                        continue;
                    }
                    let started = self.step(step, idx, &Op::Start(model, candidate.decision.clone()))?;
                    prop_assert!(started.is_some(), "{what}: the start was refused");
                    prop_assert!(self.step(step, idx, &Op::Claim)?.is_some(), "{what}: the claim was refused");
                    match outcome {
                        LaunchOutcome::Failed(reason, scope) => {
                            let before_failure = planner.ledgers().clone();
                            self.launch_failures.failed(&candidate.task, scope, reason.clone(), now);
                            // Model daemon restart/persistence at every failure boundary.
                            self.launch_failures =
                                serde_json::from_str(&serde_json::to_string(&self.launch_failures).unwrap()).unwrap();
                            prop_assert_eq!(planner.ledgers(), &before_failure, "failed start reserved budget");
                            prop_assert!(self.step(step, idx, &Op::LaunchFailed(reason))?.is_some(), "{what}: crash refused");
                            let state = self.tasks[idx].task.state;
                            prop_assert!(
                                matches!(state, TaskState::Crashed | TaskState::Failed),
                                "{what}: {state:?} after a failed start"
                            );
                            prop_assert!(!self.tasks[idx].session_live(), "{what}: live session after a failed start");
                        }
                        outcome @ (LaunchOutcome::ApprovalLost | LaunchOutcome::RouteChanged)
                            if matches!(self.tasks[idx].task.source, TaskSource::GitHub { .. }) =>
                        {
                            let op = if matches!(outcome, LaunchOutcome::RouteChanged) {
                                let project = if self.tasks[idx].task.execution_project == "default" { "api" } else { "default" };
                                Op::SourceAdmission(
                                    project.into(),
                                    crate::domain::GitHubAdmission {
                                        approved: true,
                                        reason: "approval still valid; source mapping changed".into(),
                                    },
                                )
                            } else {
                                Op::Admission(crate::domain::GitHubAdmission::denied("revoked during setup"))
                            };
                            self.step(step, idx, &op)?;
                            prop_assert!(
                                self.step(step, idx, &Op::Launched { answered: false })?.is_none(),
                                "revoked launch accepted"
                            );
                            prop_assert!(self.step(step, idx, &Op::HoldApproval)?.is_some());
                            prop_assert_eq!(self.tasks[idx].task.state, TaskState::Queued);
                            prop_assert!(!self.tasks[idx].session_live());
                        }
                        outcome => {
                            let answered = matches!(outcome, LaunchOutcome::Launched { answered: true });
                            prop_assert!(self.step(step, idx, &Op::Launched { answered })?.is_some(), "{what}: launch refused");
                            prop_assert_eq!(self.tasks[idx].task.state, TaskState::Running, "{}", what);
                            self.launch_failures.succeeded(&candidate.task);
                            planner.started(&self.cfg.budget, &candidate);
                            pass.starts += 1;
                        }
                    }
                }
            }
        }

        prop_assert_eq!(&self.ledgers, &at_start, "pass reservations leaked into persisted usage");
        // Slots.
        prop_assert!(pass.starts <= slots, "step {step} pass: {} starts for {slots} slots", pass.starts);
        // Budget: every ledger the pass reserved on stays within the
        // safety margin (each start was decided against the reservations
        // before it).
        for (provider, after) in planner.ledgers().iter() {
            let before = at_start.get(provider).expect("the pass keeps the providers");
            if after == before {
                continue;
            }
            prop_assert!(
                after.total_period_weighted >= before.total_period_weighted,
                "step {step} pass: {provider} spend went down"
            );
            prop_assert!(
                after.period_fraction() <= margin + 1e-9,
                "step {step} pass: {provider} at {:.3} of its period allowance after the pass (margin {margin:.3}, was {:.3})",
                after.period_fraction(),
                before.period_fraction()
            );
            if after.has_window() {
                prop_assert!(
                    after.window_fraction() <= margin + 1e-9,
                    "step {step} pass: {provider} at {:.3} of its window after the pass (margin {margin:.3}, was {:.3})",
                    after.window_fraction(),
                    before.window_fraction()
                );
            }
        }
        // Progress: with global and queue slots to spare, every ready task was
        // handed out (started, failed to start, or throttled).
        if pass.starts < slots {
            for id in &ready {
                let task = &self.tasks.iter().find(|t| t.task.id == *id).unwrap().task;
                if !self.launch_failures.permits(&task.execution_project, now) || !self.queue_room(task) {
                    continue;
                }
                prop_assert!(
                    pass.handed.iter().any(|(h, ..)| h == id),
                    "step {step} pass: a slot was free but {id} was not considered"
                );
            }
        }
        Ok(pass)
    }

    /// Run one op; `Ok(false)` when it was skipped.
    fn apply(&mut self, step: usize, op: &DaemonOp) -> Result<bool, TestCaseError> {
        match op {
            DaemonOp::Task(i, op) => {
                let idx = i % self.tasks.len();
                Ok(self.step(step, idx, op)?.is_some())
            }
            DaemonOp::LaunchPass(outcomes) => {
                // The store's row order must not matter: the same pass over
                // the tasks listed backwards hands out the same candidates
                // with the same verdicts (unless `pick_next` has a tie).
                let reversed = (!self.has_ties()).then(|| {
                    let mut world = self.clone();
                    world.tasks.reverse();
                    world
                });
                let pass = self.launch_pass(step, outcomes)?;
                if let Some(mut reversed) = reversed {
                    let other = reversed.launch_pass(step, outcomes)?;
                    prop_assert_eq!(&pass, &other, "step {} pass: the order of the rows changed the pass", step);
                }
                Ok(true)
            }
            DaemonOp::SpendRecorded { task, tokens, age_secs } => {
                let idx = task % self.tasks.len();
                let Some(session) = self.tasks[idx].session.as_ref() else { return Ok(false) };
                // Late transcript tails may belong to an already ended session.
                if self.budget_store.get_session(session.id).unwrap().is_none() {
                    self.budget_store.insert_session(session).unwrap();
                }
                let usage = crate::domain::UsageRecord {
                    session_id: session.id,
                    task_id: session.task_id,
                    message_id: format!("{}-{step}", session.id),
                    model_id: session.model.to_string(),
                    tier: session.model.clone(),
                    usage: crate::domain::TokenUsage { input_tokens: *tokens, ..Default::default() },
                    timestamp: (self.now - Duration::seconds(*age_secs)).max(session.started_at.min(self.now)),
                };
                prop_assert!(self.budget_store.record_usage(&usage).unwrap());
                prop_assert!(!self.budget_store.record_usage(&usage).unwrap(), "transcript replay charged twice");
                self.refresh_ledgers();
                Ok(true)
            }
            DaemonOp::Observation(provider, fraction, reset) => {
                let obs = crate::budget::probe::ObservedUsage {
                    window_used: Some(*fraction),
                    window_resets_at: reset.then_some(self.now + Duration::hours(5)),
                    ..crate::budget::probe::ObservedUsage::empty(self.now)
                };
                crate::budget::io::save_observed(&self.budget_store, *provider, &obs).unwrap();
                self.refresh_ledgers();
                Ok(true)
            }
            DaemonOp::ReloadConcurrency(max) => {
                self.cfg.scheduler.max_concurrent = *max;
                Ok(true)
            }
            DaemonOp::ReloadPolicy(project, document) => {
                let rules = PriorityRules::parse(document).unwrap();
                for idx in 0..self.tasks.len() {
                    let t = &self.tasks[idx];
                    if t.task.execution_project != *project || t.task.state.has_live_session() || t.session_live() {
                        continue;
                    }
                    let eval = rules.evaluate(&t.task, self.now, None, 0.0, 0.0);
                    self.step(step, idx, &Op::PolicyEvaluation(eval))?;
                }
                self.rules.insert(project.clone(), rules);
                Ok(true)
            }
            DaemonOp::SpecialLabel(i, enabled) => {
                let idx = i % self.tasks.len();
                self.tasks[idx].task.labels.retain(|l| l != "model/special");
                if *enabled {
                    self.tasks[idx].task.labels.push("model/special".into());
                }
                Ok(true)
            }
            DaemonOp::ProjectSettings(id, settings) => {
                self.set_project(id, settings);
                Ok(true)
            }
            DaemonOp::PauseScheduling => {
                self.scheduling_paused = true;
                Ok(true)
            }
            DaemonOp::ResumeScheduling => {
                self.scheduling_paused = false;
                Ok(true)
            }
            DaemonOp::Tick(secs) => {
                self.now += Duration::seconds(*secs);
                self.rate_limits.clear_expired(self.now);
                Ok(true)
            }
        }
    }
}

/// A task row with a session row the daemon could hold together: a retry
/// time only while crashed or throttled (and within the model's horizon),
/// a watch that is not parked while in review, a live session exactly when
/// the state expects one (either way when it may keep one), whose attempt
/// is the task's. Biased toward rows a pass can act on: mostly queued, few
/// waiting on a link or a sub-issue.
fn daemon_task() -> impl Strategy<Value = TaskWorld> {
    let state = prop_oneof![
        8 => Just(TaskState::Queued),
        2 => Just(TaskState::Crashed),
        2 => Just(TaskState::Throttled),
        2 => Just(TaskState::Running),
        1 => Just(TaskState::Idle),
        1 => Just(TaskState::Starting),
        1 => Just(TaskState::Paused),
        1 => Just(TaskState::Blocked),
        1 => Just(TaskState::NeedsAttention),
        1 => Just(TaskState::InReview),
        1 => Just(TaskState::Completed),
        1 => Just(TaskState::Failed),
        1 => Just(TaskState::Cancelled),
    ];
    state
        .prop_flat_map(|state| (task_in(state), prop::bool::weighted(0.75), prop::option::weighted(0.5, 0i64..6 * 3600)))
        .prop_map(|(mut t, unlinked, retry_in)| {
            let worktree = format!("/wt/{}/{}", t.execution_project, t.slug());
            if t.worktree_path.is_some() {
                t.worktree_path = Some(worktree.clone());
            }
            if let Some(review) = t.review.as_mut()
                && review.worktree_path.is_some()
            {
                review.worktree_path = Some(worktree);
            }
            if t.attempts > 0 {
                t.execution_repo = Some(format!("/repos/{}", t.execution_project));
            }
            if unlinked {
                t.blocked_by.clear();
                t.children.clear();
            }
            t.not_before = match t.state {
                TaskState::Crashed | TaskState::Throttled => retry_in.map(|secs| origin() + Duration::seconds(secs)),
                _ => None,
            };
            if t.state == TaskState::InReview
                && let Some(w) = t.review.as_mut()
            {
                w.parked = false;
            }
            if t.state.may_keep_session() {
                t.attempts = t.attempts.max(1);
            }
            t
        })
        .prop_flat_map(with_source_admission)
        .prop_flat_map(|t| {
            let session = if t.state.has_live_session() {
                session_for(&t).prop_map(Some).boxed()
            } else {
                prop::option::of(session_for(&t)).boxed()
            };
            (Just(t), session)
        })
        .prop_map(|(task, session)| TaskWorld { task, session, start: None, claimed: false })
}

fn daemon_world() -> impl Strategy<Value = DaemonWorld> {
    (
        vec(daemon_task(), 1..=4),
        scheduler_config(),
        budget_config(),
        rate_limits(),
        1u32..=3,
        prop::array::uniform3(execution_project_settings()),
    )
        .prop_map(|(mut tasks, scheduler, budget, rate_limits, max_concurrent, projects)| {
            let mut seen = HashSet::new();
            tasks.retain(|t| seen.insert(t.task.id));
            // The store enforces distinct keys and session IDs. Preserve that
            // constraint even when independent generators shrink to equal text/IDs.
            for (i, task) in tasks.iter_mut().enumerate() {
                task.task.key = format!("{}-{i}", task.task.key);
                if let Some(session) = task.session.as_mut() {
                    session.id = task.task.id.0;
                }
            }
            let cfg = Config { scheduler: SchedulerConfig { max_concurrent, ..scheduler }, budget, ..Config::default() };
            let mut world = DaemonWorld {
                cfg,
                rules: HashMap::new(),
                checkouts: HashMap::new(),
                now: origin(),
                tasks,
                ledgers: Ledgers::default(),
                budget_store: crate::store::Store::open_in_memory().unwrap(),
                rate_limits,
                estimator: Estimator::from_summaries(&[]),
                scheduling_paused: false,
                launch_failures: Default::default(),
            };
            for task in &world.tasks {
                world.budget_store.insert_task(&task.task).unwrap();
            }
            world.refresh_ledgers();
            for (id, settings) in ["default", "api", "web"].into_iter().zip(projects) {
                world.set_project(id, &settings);
            }
            world
        })
}

fn check_daemon(step: usize, op: &DaemonOp, before: &DaemonWorld, after: &DaemonWorld) -> Result<(), TestCaseError> {
    for (idx, (b, a)) in before.tasks.iter().zip(&after.tasks).enumerate() {
        let may_change = match op {
            DaemonOp::Task(target, _) | DaemonOp::SpecialLabel(target, _) => idx == target % before.tasks.len(),
            DaemonOp::ReloadPolicy(project, _) => &b.task.execution_project == project,
            DaemonOp::LaunchPass(_) => before.rules.contains_key(&b.task.execution_project),
            _ => false,
        };
        if !may_change {
            prop_assert_eq!(&a.task, &b.task, "another task or project configuration changed this task");
            prop_assert_eq!(&a.session, &b.session);
        }
    }
    let max = after.cfg.scheduler.max_concurrent;
    let (live_before, live_after) = (before.live_count(), after.live_count());
    // Sessions only start through a pass, which never goes beyond
    // `max_concurrent` (a lowered limit is reached by attrition).
    prop_assert!(
        live_after <= live_before.max(max),
        "step {step} {op:?}: {live_after} live sessions for max_concurrent {max} ({live_before} before)"
    );
    if matches!(op, DaemonOp::LaunchPass(_)) && before.scheduling_paused {
        let was: Vec<&Task> = before.tasks.iter().map(|t| &t.task).collect();
        let is: Vec<&Task> = after.tasks.iter().map(|t| &t.task).collect();
        prop_assert_eq!(was, is, "step {} pass while scheduling is paused changed a task", step);
    }
    let ids: HashSet<TaskId> = after.tasks.iter().map(|t| t.task.id).collect();
    prop_assert_eq!(ids.len(), after.tasks.len(), "two rows for one task");
    Ok(())
}

proptest! {
    #![proptest_config(model_config(128))]

    /// Any interleaving of launch passes, per-task calls, recorded usage,
    /// the scheduling pause and time keeps every task consistent and the
    /// passes within slots and budget (see the module doc).
    #[test]
    fn the_daemon_survives_any_sequence_of_ticks(world in daemon_world(), ops in vec(daemon_op(), 1..model_steps())) {
        let mut world = world;
        for (step, op) in ops.iter().enumerate() {
            let before = world.clone();
            if !world.apply(step, op)? {
                continue;
            }
            check_daemon(step, op, &before, &world)?;
        }
    }
}

proptest! {
    /// Interleave two task rows and two readers with the real SQLite guards.
    /// A saved candidate may outlive several reassignments before its launch claim.
    #[test]
    fn stale_project_candidates_cannot_claim_another_repository(
        ops in vec((0u8..3, 0usize..2, execution_project()), 1..50)
    ) {
        let store = crate::store::Store::open_in_memory().unwrap();
        let mut rows: Vec<_> = ["A", "B"].into_iter().map(|key| {
            let mut task = Task::new(key, key, TaskSource::Manual);
            task.project = Some("independent source project".into());
            store.insert_task(&task).unwrap();
            store.get_task(task.id).unwrap().unwrap()
        }).collect();
        let mut snapshots = rows.clone();
        for (op, idx, target) in ops {
            let mut expected = rows.clone();
            match op {
                0 => snapshots[idx] = rows[idx].clone(),
                1 => {
                    let accepted = rows[idx].state == TaskState::Queued;
                    prop_assert_eq!(store.set_execution_project(rows[idx].id, &rows[idx].execution_project, &target).is_ok(), accepted);
                    if accepted { expected[idx].project_assignment = Some(crate::routing::ProjectAssignment::Explicit { project: target.clone() }); expected[idx].execution_project = target; expected[idx].routing = None; }
                }
                _ => {
                    let saved = &snapshots[idx];
                    if saved.state != TaskState::Queued { continue; }
                    let mut starting = saved.clone();
                    starting.state = TaskState::Starting;
                    starting.model = Some(ModelTier::sonnet());
                    let accepted = rows[idx].state == saved.state && rows[idx].execution_project == saved.execution_project && rows[idx].project_assignment == saved.project_assignment && rows[idx].routing == saved.routing;
                    prop_assert_eq!(store.claim_task_start(saved, &starting).unwrap(), accepted);
                    if accepted {
                        expected[idx].state = TaskState::Starting;
                        expected[idx].model = starting.model;
                    }
                }
            }
            for i in 0..rows.len() {
                rows[i] = store.get_task(rows[i].id).unwrap().unwrap();
                expected[i].updated_at = rows[i].updated_at;
                prop_assert_eq!(&rows[i], &expected[i], "claim/reassignment changed another field or task");
            }
        }
    }
}

proptest! {
    /// SQLite must reject a candidate whose approval or scope changed while it waited.
    /// Reading after a change/restart can restore eligibility only with approved evidence.
    #[test]
    fn stale_admission_candidates_cannot_claim(
        ops in vec((0u8..6, admission_decision()), 1..60)
    ) {
        let store = crate::store::Store::open_in_memory().unwrap();
        let mut task = Task::new("acme/app#1", "scope", TaskSource::GitHub {
            repository: "acme/app".into(), number: 1, url: "https://github.com/acme/app/issues/1".into()
        });
        store.insert_task(&task).unwrap();
        let mut snapshot = task.clone();
        for (op, admission) in ops {
            match op {
                0 => snapshot = store.get_task(task.id).unwrap().unwrap(),
                1 => {
                    transitions::on_github_admission(&mut task, admission);
                    store.update_task(&task).unwrap();
                }
                2 => {
                    task.description.push('x');
                    transitions::on_github_admission(&mut task, crate::domain::GitHubAdmission::denied("material edit"));
                    store.update_task(&task).unwrap();
                }
                3 => {
                    // A restarted reader reloads persisted evidence; it cannot synthesize approval.
                    snapshot = store.get_task(task.id).unwrap().unwrap();
                    prop_assert_eq!(snapshot.execution_approved(), task.execution_approved());
                }
                4 => {
                    transitions::on_github_admission(&mut task, crate::domain::GitHubAdmission::denied("revoked in setup"));
                    transitions::on_github_launch_held(&mut task);
                    store.update_task(&task).unwrap();
                }
                _ => {
                    if snapshot.state != TaskState::Queued { continue; }
                    let mut starting = snapshot.clone();
                    starting.state = TaskState::Starting;
                    let accepted = snapshot.execution_approved() && task.execution_approved()
                        && task.state == snapshot.state && task.description == snapshot.description
                        && task.github_admission == snapshot.github_admission;
                    prop_assert_eq!(store.claim_task_start(&snapshot, &starting).unwrap(), accepted);
                    if accepted { task.state = TaskState::Starting; }
                    let saved = store.get_task(task.id).unwrap().unwrap();
                    prop_assert_eq!(saved.state, task.state);
                    prop_assert_eq!(saved.github_admission, task.github_admission.clone());
                }
            }
        }
    }
}

proptest! {
    /// A review/retry keeps its checkout pin but must persist its NEW worktree
    /// claim before the post-setup GitHub approval read. An unchanged binding
    /// cannot be relied on to trigger a separate full-row write.
    #[test]
    fn github_relaunch_claim_survives_admission_reload(
        mut task in github_task(), previous_attempts in 0u32..5,
        decision in decision(), refreshed in admission_decision()
    ) {
        let store = crate::store::Store::open_in_memory().unwrap();
        let checkout = format!("/repos/{}", task.execution_project);
        task.state = TaskState::Queued;
        task.execution_repo = Some(checkout.clone());
        task.attempts = previous_attempts;
        task.worktree_path = None;
        task.github_admission = Some(crate::domain::GitHubAdmission { approved: true, reason: "verified".into() });
        store.insert_task(&task).unwrap();
        let before = store.get_task(task.id).unwrap().unwrap();
        let mut starting = before.clone();
        let root = std::path::PathBuf::from("/wt").join(&task.execution_project);
        let start = launch::on_starting(&mut starting, &ModelTier::sonnet(), &decision, &Config::default(), &root, true).unwrap();
        launch::claim(&mut starting, &start);
        prop_assert!(store.claim_task_start(&before, &starting).unwrap());
        prop_assert!(commands::bind_project(&mut starting, &checkout).unwrap().is_empty());
        let mut loaded = store.get_task(task.id).unwrap().unwrap();
        prop_assert_eq!(loaded.attempts, previous_attempts + 1);
        prop_assert_eq!(&loaded.branch, &starting.branch);
        prop_assert_eq!(&loaded.worktree_path, &starting.worktree_path);
        let before_admission = loaded.clone();
        transitions::on_github_admission(&mut loaded, refreshed);
        prop_assert!(store.record_github_admission(&before_admission, &loaded).unwrap());
        let saved = store.get_task(task.id).unwrap().unwrap();
        prop_assert_eq!(saved.attempts, start.attempt);
        prop_assert_eq!(saved.worktree_path, Some(start.worktree.display().to_string()));
        prop_assert_eq!(saved.branch, Some(start.branch));
        prop_assert_eq!(saved.execution_repo, Some(checkout));
        prop_assert_eq!(saved.github_admission, loaded.github_admission);
    }
}

mod analysis;
mod deferral;
mod policy_reload;
mod triage;

mod background_setup;
