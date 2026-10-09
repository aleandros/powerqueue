//! Model-based test of a task's life (proptest).
//!
//! A random sequence of the calls the daemon makes into the pure core
//! (commands, launches, hooks, probes, the PR watcher, re-scoring) is run
//! against one task, with the preconditions of each call mirroring the
//! daemon's call site. After every step the task, its session and the
//! effects are checked against invariants no single transition test can
//! see: a live session only exists in states that expect one, `not_before`
//! is only set while crashed or throttled, attempts move only through
//! `claim` and a retry, terminal states are only left by a retry, review
//! rounds stay within `review_rounds_max`, and every effect serialises and
//! deserialises to itself (the trace a specification would consume).
//!
//! The ops are generated without preconditions and skipped when one does
//! not hold, which keeps the generator simple and lets shrinking drop any
//! op.

use chrono::{DateTime, Duration, Utc};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;

use crate::budget::Decision;
use crate::config::Config;
use crate::domain::{Criticality, LinkedIssue, ModelTier, Session, SessionState, Task, TaskSource, TaskState};
use crate::github::PrStatus;
use crate::priority::Evaluation;
use crate::session::{HookOutcome, SessionProbe};
use crate::strategies::*;

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
    /// `powerqueue task complete` (no PR).
    Complete(Option<String>),
    /// `powerqueue task complete --pr <url>`.
    HandOff(String),
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

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        2 => Just(Op::Pause),
        3 => Just(Op::Resume),
        1 => Just(Op::Cancel),
        3 => Just(Op::Retry),
        1 => prop::option::of(claude_model()).prop_map(Op::SetModel),
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
        8 => hook_outcome().prop_map(Op::Hook),
        6 => (session_probe(), any::<bool>(), pane_tail(), prop::option::of(60i64..7200))
            .prop_map(|(probe, nudged, pane_tail, cooldown_secs)| Op::Probe { probe, nudged, pane_tail, cooldown_secs }),
        2 => prop::option::of(line()).prop_map(Op::Complete),
        3 => any_pr_url().prop_map(Op::HandOff),
        4 => pr_status(1).prop_map(Op::PrStatus),
        1 => (vec(linked_issue(), 0..3), vec(linked_issue(), 0..2))
            .prop_map(|(blocked_by, children)| Op::Links { blocked_by, children }),
        3 => (criticality(), 0u32..2000, prop::bool::weighted(0.2))
            .prop_map(|(criticality, score, skip)| Op::Evaluate { criticality, score: score as f64, skip }),
        2 => Just(Op::AnswerQueued),
        1 => Just(Op::ContainerClosed),
        4 => (1i64..3 * 3600).prop_map(Op::Tick),
    ]
}

/// Everything the daemon holds for one task.
struct World {
    cfg: Config,
    now: DateTime<Utc>,
    task: Task,
    /// The latest session row, live or not.
    session: Option<Session>,
    /// A start under way (`on_starting` done, launch not finished).
    start: Option<launch::Start>,
    /// `claim` ran for `start`.
    claimed: bool,
}

impl World {
    fn new(cfg: Config, task: Task) -> Self {
        Self { cfg, now: origin(), task, session: None, start: None, claimed: false }
    }

    fn session_live(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.state.is_live())
    }

    /// What the planner hands out: schedulable, waiting on nothing, retry time passed, no live session.
    fn can_start(&self) -> bool {
        self.task.state.is_schedulable()
            && !self.task.is_waiting()
            && self.task.not_before.is_none_or(|t| t <= self.now)
            && !self.session_live()
            && self.start.is_none()
    }

    /// Run one op; `None` when its precondition does not hold (skipped).
    fn apply(&mut self, op: &Op) -> Option<Vec<Effect>> {
        let now = self.now;
        let live = self.session_live();
        // A start runs to its end inside one daemon tick (`start_task`):
        // nothing else sees the task between `on_starting` and the launch
        // or the crash that prevents it.
        if self.start.is_some() && !matches!(op, Op::Claim | Op::LaunchFailed(_) | Op::Launched { .. }) {
            return None;
        }
        match op {
            Op::Pause => Some(commands::on_pause(&mut self.task, live)),
            Op::Resume => Some(commands::on_resume(&mut self.task, live, now)),
            Op::Cancel => Some(commands::on_cancel(&mut self.task, self.session.as_mut(), now)),
            Op::Retry => Some(commands::on_retry(&mut self.task, self.session.as_mut(), now)),
            Op::SetModel(m) => Some(commands::on_set_model(&mut self.task, m.clone())),
            Op::Throttle(d) => self.can_start().then(|| launch::on_throttled(&mut self.task, d, now)),
            Op::Start(model, d) => {
                if !self.can_start() {
                    return None;
                }
                let resuming = self.task.review_relaunch().is_some();
                let start = launch::on_starting(&mut self.task, model, d, &self.cfg, std::path::Path::new("/wt"), resuming);
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
                Some(transitions::on_crash(&mut self.task, None, reason, None, &self.cfg.scheduler, now, None))
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
                let effects = launch::on_launched(&mut self.task, &session, &facts, &self.cfg, now);
                self.session = Some(session);
                Some(effects)
            }
            Op::Hook(outcome) => {
                let session = self.session.as_mut()?;
                Some(transitions::on_hook_outcome(&mut self.task, session, outcome, &self.cfg, now, now + Duration::days(3)))
            }
            Op::Probe { probe, nudged, pane_tail, cooldown_secs } => {
                if !live {
                    return None;
                }
                let session = self.session.as_mut()?;
                let ctx = ProbeContext {
                    now,
                    nudged: *nudged,
                    pane_tail: pane_tail.clone(),
                    provider_cooldown_until: cooldown_secs.map(|s| now + Duration::seconds(s)),
                };
                Some(transitions::on_probe(&mut self.task, session, probe, &self.cfg.scheduler, &ctx))
            }
            Op::Complete(summary) => {
                // `cli::commands::task::complete_task`, then the daemon's finalize.
                if self.task.state == TaskState::Completed || !self.task.state.can_transition_to(TaskState::Completed) {
                    return None;
                }
                self.task.state = TaskState::Completed;
                self.task.completed_at = Some(now);
                self.task.not_before = None;
                if let Some(s) = summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    self.task.summary = Some(s.to_string());
                }
                self.task.last_error = None;
                Some(self.finalize())
            }
            Op::HandOff(url) => {
                // `cli::commands::task::hand_off_for_review`, then the daemon's finalize.
                if review::pr_number_of(url).is_none()
                    || (self.task.state != TaskState::InReview && !self.task.state.can_transition_to(TaskState::InReview))
                {
                    return None;
                }
                self.task.hand_off_for_review(url, now);
                Some(self.finalize())
            }
            Op::PrStatus(status) => {
                if self.task.state != TaskState::InReview || live {
                    return None;
                }
                let number = self.task.pr_url.as_deref().and_then(review::pr_number_of)?;
                let status = PrStatus { number, ..status.clone() };
                Some(review::on_pr_status(&mut self.task, &status, &self.cfg.scheduler, now))
            }
            Op::Links { blocked_by, children } => {
                self.task.blocked_by = blocked_by.clone();
                self.task.children = children.clone();
                Some(Vec::new())
            }
            Op::Evaluate { criticality, score, skip } => {
                if self.task.state.has_live_session() {
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
            Op::Tick(secs) => {
                self.now = now + Duration::seconds(*secs);
                Some(Vec::new())
            }
        }
    }

    /// The daemon's finalize phase: a live session of a task that was
    /// handed off or finished outside the hook path is released.
    fn finalize(&mut self) -> Vec<Effect> {
        match self.session.as_mut() {
            Some(s) if s.state.is_live() => transitions::release_session(&self.task, s, self.now),
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

/// States in which a live session may exist: the ones that expect one,
/// plus a paused task (its session finishes its turn) and a throttled one
/// (the agent waits in-session for its usage limit to reset).
fn may_hold_live_session(state: TaskState) -> bool {
    state.has_live_session() || matches!(state, TaskState::Paused | TaskState::Throttled)
}

fn check(step: usize, op: &Op, before: &World, after: &World, effects: &[Effect]) -> Result<(), TestCaseError> {
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

    // Session / task agreement.
    if let Some(s) = &after.session {
        prop_assert_eq!(s.task_id, a.id);
        if s.state.is_live() {
            prop_assert!(may_hold_live_session(a.state), "{}", ctx("live session in a state that has none"));
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

    // Terminal states are only left by a retry.
    if b.state.is_terminal() && !a.state.is_terminal() {
        prop_assert!(matches!(op, Op::Retry), "{}", ctx("left a terminal state"));
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
        prop_assert!(w.rounds <= after.cfg.scheduler.review_rounds_max.max(b.review.as_ref().map_or(0, |r| r.rounds)));
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
        let config = Config { scheduler: cfg, ..Config::default() };
        let mut world = World::new(config, task);
        for (step, op) in ops.iter().enumerate() {
            let before = World {
                cfg: world.cfg.clone(),
                now: world.now,
                task: world.task.clone(),
                session: world.session.clone(),
                start: world.start.clone(),
                claimed: world.claimed,
            };
            let Some(effects) = world.apply(op) else { continue };
            check(step, op, &before, &world, &effects)?;
        }
    }
}
