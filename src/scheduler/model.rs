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
//! unconsidered while a slot is free (bounded progress), and that the
//! order the store lists the tasks in does not change the pass.
//!
//! The ops are generated without preconditions and skipped when one does
//! not hold, which keeps the generator simple and lets shrinking drop any
//! op.

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;

use crate::budget::{Decision, Estimator, Ledgers, Period, RateLimitState, WINDOW_RECHECK};
use crate::config::{Config, SchedulerConfig};
use crate::domain::{Criticality, LinkedIssue, ModelTier, Session, SessionState, Task, TaskId, TaskSource, TaskState};
use crate::github::PrStatus;
use crate::priority::{Evaluation, PriorityRules};
use crate::session::{HookOutcome, SessionProbe};
use crate::strategies::*;

use super::launch::{LaunchContext, LaunchPlanner};
use super::transitions::{Effect, ProbeContext};
use super::{commands, launch, review, transitions};

/// One call into the core, as the daemon or the CLI would make it.
#[derive(Debug, Clone)]
enum Op {
    Pause,
    Resume,
    Cancel,
    Retry,
    SetModel(Option<ModelTier>),
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
    /// PRIORITY.md re-scored the task.
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
        2 => Just(Op::Pause),
        3 => Just(Op::Resume),
        1 => Just(Op::Cancel),
        3 => Just(Op::Retry),
        1 => prop::option::of(claude_model()).prop_map(Op::SetModel),
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
            && !self.task.is_waiting()
            && self.task.not_before.is_none_or(|t| t <= now)
            && !self.session_live()
            && self.start.is_none()
    }

    /// Run one op; `None` when its precondition does not hold (skipped).
    /// `Tick` is the world's business and is never applied here.
    fn apply(&mut self, cfg: &Config, now: DateTime<Utc>, op: &Op) -> Option<Vec<Effect>> {
        let live = self.session_live();
        // A start runs to its end inside one daemon tick (`start_task`):
        // no other daemon call sees the task between `on_starting` and the
        // launch or the crash that prevents it. The CLI's direct writes run
        // in another process and do see it; they must refuse (checked).
        let direct = matches!(op, Op::Complete(_) | Op::HandOff(_) | Op::Block(_));
        if self.start.is_some() && !direct && !matches!(op, Op::Claim | Op::LaunchFailed(_) | Op::Launched { .. }) {
            return None;
        }
        match op {
            Op::Pause => Some(commands::on_pause(&mut self.task, live)),
            Op::Resume => Some(commands::on_resume(&mut self.task, live, now)),
            Op::Cancel => Some(commands::on_cancel(&mut self.task, self.session.as_mut(), now)),
            Op::Retry => Some(commands::on_retry(&mut self.task, self.session.as_mut(), now)),
            Op::SetModel(m) => Some(commands::on_set_model(&mut self.task, m.clone())),
            Op::Throttle(d) => self.can_start(now).then(|| launch::on_throttled(&mut self.task, d, now)),
            Op::Start(model, d) => {
                if !self.can_start(now) {
                    return None;
                }
                let resuming = self.task.review_relaunch().is_some();
                let start = launch::on_starting(&mut self.task, model, d, cfg, std::path::Path::new("/wt"), resuming);
                let effects = start.effects.clone();
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
                if !self.claimed {
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
    (word(), criticality(), 0.0f64..2000.0, prop::option::of(1u32..=4), vec(word(), 0..2)).prop_map(
        |(key, criticality, score, max_attempts, labels)| {
            let mut t = Task::new(format!("ENG-{key}"), "t", TaskSource::Manual);
            t.criticality = criticality;
            t.score = score;
            t.max_attempts = max_attempts;
            t.labels = labels;
            t.created_at = origin() - Duration::hours(1);
            t.updated_at = t.created_at;
            t
        },
    )
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
    let ctx = |what: &str| {
        format!(
            "step {step} {op:?}: {what}\n  before: {:?} {:?}\n  after:  {:?} {:?}",
            b.state, b.not_before, a.state, a.not_before
        )
    };

    // Legal moves. Closing a container (a parent issue whose sub-issues are
    // done) is the one move the table does not list: it happens from any
    // open state and must not open `task complete` to queued tasks.
    if !matches!(op, Op::ContainerClosed) {
        prop_assert!(b.state.can_transition_to(a.state), "{}", ctx("move not in TaskState::can_transition_to"));
    }

    // A launch under way is never interrupted: `start_task` persists
    // `running` when it ends, so a direct write landing in between would
    // be lost. Only the launch's own steps apply.
    if before.start.is_some() {
        prop_assert!(
            matches!(op, Op::Claim | Op::LaunchFailed(_) | Op::Launched { .. }),
            "{}",
            ctx("applied to a task the daemon is launching")
        );
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
    if matches!(op, Op::Evaluate { .. }) {
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
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Any sequence of daemon calls keeps the task, its session and the
    /// effect trace consistent (see the module doc for the invariants).
    #[test]
    fn a_task_survives_any_sequence_of_daemon_calls(task in fresh_task(), cfg in scheduler_config(), ops in vec(op(), 1..40)) {
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

// ------------------------------------------------------------ whole daemon

/// How a start that the planner handed out ends in `start_task`.
#[derive(Debug, Clone)]
enum LaunchOutcome {
    /// The session launched (`on_launched`).
    Launched { answered: bool },
    /// The worktree or the launcher failed first (`on_crash` without a session).
    Failed(String),
}

fn launch_outcome() -> impl Strategy<Value = LaunchOutcome> {
    prop_oneof![
        3 => any::<bool>().prop_map(|answered| LaunchOutcome::Launched { answered }),
        1 => line().prop_map(LaunchOutcome::Failed),
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
    /// Usage of a live session reached the store (`Ledger::add_spend`).
    SpendRecorded {
        task: usize,
        weighted: f64,
    },
    PauseScheduling,
    ResumeScheduling,
    Tick(i64),
}

fn daemon_op() -> impl Strategy<Value = DaemonOp> {
    prop_oneof![
        40 => (0usize..4, task_op()).prop_map(|(i, op)| DaemonOp::Task(i, op)),
        20 => vec(launch_outcome(), 0..4).prop_map(DaemonOp::LaunchPass),
        4 => (0usize..4, 0u64..2_000_000).prop_map(|(task, w)| DaemonOp::SpendRecorded { task, weighted: w as f64 }),
        1 => Just(DaemonOp::PauseScheduling),
        2 => Just(DaemonOp::ResumeScheduling),
        6 => (1i64..3 * 3600).prop_map(DaemonOp::Tick),
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
#[derive(Debug, Clone)]
struct DaemonWorld {
    cfg: Config,
    rules: PriorityRules,
    now: DateTime<Utc>,
    tasks: Vec<TaskWorld>,
    /// The stored usage (what `Ledgers::load` returns at the next pass).
    ledgers: Ledgers,
    rate_limits: RateLimitState,
    estimator: Estimator,
    scheduling_paused: bool,
}

impl DaemonWorld {
    fn live_count(&self) -> u32 {
        self.tasks.iter().filter(|t| t.session_live()).count() as u32
    }

    fn ready(&self) -> Vec<TaskId> {
        self.tasks.iter().filter(|t| t.can_start(self.now)).map(|t| t.task.id).collect()
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

    /// What `Ledgers::load` would return now: the ledgers stamped with the
    /// current time, a period that has ended rolled over with nothing
    /// spent in the new one yet, the window ending now. Window spend is
    /// kept while the new window overlaps the old one (the model does not
    /// date its spends) and dropped once the whole window has passed.
    fn refresh_ledgers(&mut self) {
        let now = self.now;
        for ledger in self.ledgers.by_provider.values_mut() {
            ledger.now = now;
            let new_period = now >= ledger.period.end;
            if new_period {
                let len = ledger.period.len();
                while now >= ledger.period.end {
                    ledger.period = Period { start: ledger.period.end, end: ledger.period.end + len };
                }
                for t in &mut ledger.tiers {
                    t.period_weighted = 0.0;
                }
                ledger.total_period_weighted = 0.0;
                ledger.spent_since_observation = 0.0;
            }
            let window = Period { start: now - Duration::hours(5), end: now };
            if new_period || window.start >= ledger.window.end {
                for t in &mut ledger.tiers {
                    t.window_weighted = 0.0;
                }
                ledger.total_window_weighted = 0.0;
            }
            ledger.window = window;
        }
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
        let Some(effects) = self.tasks[idx].apply(&self.cfg, now, op) else { return Ok(None) };
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
        let mut planner = LaunchPlanner::new(self.estimator.clone(), self.ledgers.clone(), candidates, slots);
        let mut outcomes = outcomes.iter();
        loop {
            let ctx = LaunchContext { budget: &self.cfg.budget, rules: &self.rules, rate_limits: &self.rate_limits };
            let Some(candidate) = planner.next(ctx, now) else { break };
            let idx = self.tasks.iter().position(|t| t.task.id == candidate.task.id).expect("a candidate is one of the tasks");
            let what = format!(
                "step {step} pass: {} ({:?}, score {}) decided {:?}",
                candidate.task.key, candidate.task.state, candidate.task.score, candidate.decision.model
            );
            prop_assert!(!pass.handed.iter().any(|(id, ..)| *id == candidate.task.id), "{what}: handed out twice");
            prop_assert!(ready.contains(&candidate.task.id), "{what}: not ready when the pass began");
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
                    let started = self.step(step, idx, &Op::Start(model, candidate.decision.clone()))?;
                    prop_assert!(started.is_some(), "{what}: the start was refused");
                    prop_assert!(self.step(step, idx, &Op::Claim)?.is_some(), "{what}: the claim was refused");
                    match outcomes.next().cloned().unwrap_or(LaunchOutcome::Launched { answered: false }) {
                        LaunchOutcome::Failed(reason) => {
                            prop_assert!(self.step(step, idx, &Op::LaunchFailed(reason))?.is_some(), "{what}: crash refused");
                            let state = self.tasks[idx].task.state;
                            prop_assert!(
                                matches!(state, TaskState::Crashed | TaskState::Failed),
                                "{what}: {state:?} after a failed start"
                            );
                            prop_assert!(!self.tasks[idx].session_live(), "{what}: live session after a failed start");
                        }
                        LaunchOutcome::Launched { answered } => {
                            prop_assert!(self.step(step, idx, &Op::Launched { answered })?.is_some(), "{what}: launch refused");
                            prop_assert_eq!(self.tasks[idx].task.state, TaskState::Running, "{}", what);
                            planner.started(&self.cfg.budget, &candidate);
                            pass.starts += 1;
                        }
                    }
                }
            }
        }

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
        // Progress: with a slot to spare, every task that was ready was
        // handed out (started, failed to start, or throttled).
        if pass.starts < slots {
            for id in &ready {
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
            DaemonOp::SpendRecorded { task, weighted } => {
                let idx = task % self.tasks.len();
                let Some(session) = self.tasks[idx].session.as_ref().filter(|s| s.state.is_live()) else { return Ok(false) };
                let tier = session.model.clone();
                let Some(ledger) = self.ledgers.for_model_mut(&tier) else { return Ok(false) };
                ledger.add_spend(&tier, *weighted);
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
    (vec(daemon_task(), 1..=4), scheduler_config(), budget_config(), ledgers(), rate_limits(), 1u32..=3).prop_map(
        |(mut tasks, scheduler, budget, ledgers, rate_limits, max_concurrent)| {
            let mut seen = HashSet::new();
            tasks.retain(|t| seen.insert(t.task.id));
            let cfg = Config { scheduler: SchedulerConfig { max_concurrent, ..scheduler }, budget, ..Config::default() };
            DaemonWorld {
                cfg,
                rules: PriorityRules::default(),
                now: origin(),
                tasks,
                ledgers,
                rate_limits,
                estimator: Estimator::from_summaries(&[]),
                scheduling_paused: false,
            }
        },
    )
}

fn check_daemon(step: usize, op: &DaemonOp, before: &DaemonWorld, after: &DaemonWorld) -> Result<(), TestCaseError> {
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
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Any interleaving of launch passes, per-task calls, recorded usage,
    /// the scheduling pause and time keeps every task consistent and the
    /// passes within slots and budget (see the module doc).
    #[test]
    fn the_daemon_survives_any_sequence_of_ticks(world in daemon_world(), ops in vec(daemon_op(), 1..40)) {
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
