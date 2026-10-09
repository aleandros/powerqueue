//! Launch planning, pure: which task starts next and on which model, how a
//! previous session is resumed, and the task's state around a start.
//!
//! The daemon drives a [`LaunchPlanner`] one candidate at a time ([`next`],
//! then [`started`] once the session really launched, so a failed start
//! frees its slot for the next candidate in the same tick); `budget plan`
//! takes the whole plan at once ([`plan_all`]). Everything in between
//! (`git`, the launcher, tmux) stays in the daemon and reports back through
//! [`on_starting`] / [`claim`] / [`on_launched`] /
//! [`crate::scheduler::transitions::on_crash`].
//!
//! [`next`]: LaunchPlanner::next
//! [`started`]: LaunchPlanner::started
//! [`plan_all`]: LaunchPlanner::plan_all

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::budget::{Decision, Estimator, Ledgers, Policy, RateLimitState, WINDOW_RECHECK, tier_weight};
use crate::config::{BudgetConfig, Config};
use crate::domain::{EventLevel, ModelTier, ReviewRelaunch, Session, SessionState, Task, TaskId, TaskSource, TaskState};
use crate::priority::PriorityRules;
use crate::session::agent_for;
use crate::worktree::branch_name;

use super::lifecycle::{pick_next, worktree_dir};
use super::transitions::{Effect, LinearTarget};

/// The rules' preference list for a task, most wanted first: the per-task
/// override line (`KEY: model = ...`), else the first matching `## Models`
/// `if` row, else the `## Models` entry for its criticality. Empty when the
/// rules say nothing. (`task.model_override` is read by the policy itself.)
pub fn preferred_models(rules: &PriorityRules, task: &Task, now: DateTime<Utc>) -> Vec<ModelTier> {
    let evaluated = rules.evaluate(task, now, None, 0.0, 0.0).models;
    if evaluated.is_empty() { rules.model_for(task.criticality).to_vec() } else { evaluated }
}

/// Count a predicted cost against the in-memory ledger of the model's
/// provider so several launches in one pass do not each think they are the
/// only one. A model whose provider has no ledger is ignored.
pub fn reserve(ledgers: &mut Ledgers, tier: &ModelTier, weighted_cost: f64) {
    if let Some(ledger) = ledgers.for_model_mut(tier) {
        ledger.add_spend(tier, weighted_cost);
    }
}

/// One task the planner handed out, with the policy's verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub task: Task,
    /// The rules' preference list the policy was given.
    pub preferred: Vec<ModelTier>,
    pub decision: Decision,
}

impl Candidate {
    /// Predicted weighted cost on the chosen model (0 when throttled).
    pub fn weighted_cost(&self, cfg: &BudgetConfig) -> f64 {
        self.decision.model.as_ref().map_or(0.0, |m| self.decision.prediction.weighted_tokens * tier_weight(cfg, m))
    }
}

/// What the policy decides with, borrowed for each step of a pass so the
/// planner sees rate-limit marks made between two of its steps (a failed
/// start may mark a provider) and nothing is copied per tick.
#[derive(Debug, Clone, Copy)]
pub struct LaunchContext<'a> {
    pub budget: &'a BudgetConfig,
    pub rules: &'a PriorityRules,
    pub rate_limits: &'a RateLimitState,
}

/// Hands out schedulable tasks in [`pick_next`] order with a model decision
/// each, while slots remain. Owns a copy of the ledgers so reservations
/// made during the pass do not touch the stored usage.
#[derive(Debug, Clone)]
pub struct LaunchPlanner {
    estimator: Estimator,
    ledgers: Ledgers,
    candidates: Vec<Task>,
    considered: HashSet<TaskId>,
    slots: u32,
}

impl LaunchPlanner {
    /// `candidates` are the open tasks without a live session; `slots` how
    /// many sessions may still start. Expired rate-limit marks should be
    /// cleared by the caller first.
    pub fn new(estimator: Estimator, ledgers: Ledgers, candidates: Vec<Task>, slots: u32) -> Self {
        Self { estimator, ledgers, candidates, considered: HashSet::new(), slots }
    }

    /// The next task to consider and the policy's decision for it: the best
    /// schedulable task not handed out yet. `None` when no slot is left or
    /// nothing else can start now. A task is handed out once per pass
    /// whether or not it starts.
    pub fn next(&mut self, ctx: LaunchContext<'_>, now: DateTime<Utc>) -> Option<Candidate> {
        if self.slots == 0 {
            return None;
        }
        let task = {
            let remaining: Vec<Task> = self.candidates.iter().filter(|t| !self.considered.contains(&t.id)).cloned().collect();
            pick_next(&remaining, now).cloned()?
        };
        Some(self.decide(ctx, task, now))
    }

    fn decide(&mut self, ctx: LaunchContext<'_>, task: Task, now: DateTime<Utc>) -> Candidate {
        self.considered.insert(task.id);
        let prediction = self.estimator.predict(&task);
        // Rules express a *preference* (`## Models`, `KEY: model = x`); only
        // `task model <tier>` on the CLI is a hard override. Either way the
        // policy may still downgrade when the tier is out of budget.
        let preferred = preferred_models(ctx.rules, &task, now);
        let decision = Policy::new(ctx.budget, &self.ledgers, ctx.rate_limits).decide(&task, prediction, &preferred);
        Candidate { task, preferred, decision }
    }

    /// A candidate's session launched: its slot is used and its predicted
    /// cost reserved against the ledgers for the rest of the pass. A
    /// throttled decision (no model) changes nothing.
    pub fn started(&mut self, budget: &BudgetConfig, candidate: &Candidate) {
        let Some(model) = &candidate.decision.model else { return };
        self.slots = self.slots.saturating_sub(1);
        reserve(&mut self.ledgers, model, candidate.weighted_cost(budget));
    }

    /// What a simulation shows: every candidate of the pass in start order,
    /// assuming each start succeeds, then the tasks the pass would not reach
    /// (waiting on `not_before` or a dependency, or beyond the slots), each
    /// decided after the ones before it. Throttled tasks have `model = None`.
    pub fn plan_all(mut self, ctx: LaunchContext<'_>, now: DateTime<Utc>) -> Vec<Candidate> {
        let mut out = Vec::new();
        while let Some(candidate) = self.next(ctx, now) {
            self.started(ctx.budget, &candidate);
            out.push(candidate);
        }
        let rest: Vec<Task> =
            self.candidates.iter().filter(|t| t.state.is_schedulable() && !self.considered.contains(&t.id)).cloned().collect();
        for task in rest {
            let candidate = self.decide(ctx, task, now);
            self.started(ctx.budget, &candidate);
            out.push(candidate);
        }
        out
    }

    /// The ledgers with every confirmed start reserved.
    #[cfg(test)]
    pub fn ledgers(&self) -> &Ledgers {
        &self.ledgers
    }
}

/// No model fits the budget right now: the task waits until the policy's
/// `retry_at` (or [`WINDOW_RECHECK`] from now). Logged only when the task
/// was not already throttled.
pub fn on_throttled(task: &mut Task, decision: &Decision, now: DateTime<Utc>) -> Vec<Effect> {
    let retry_at = decision.retry_at.unwrap_or(now + WINDOW_RECHECK);
    let was = task.state;
    task.state = TaskState::Throttled;
    task.not_before = Some(retry_at);
    if was == TaskState::Throttled {
        return Vec::new();
    }
    vec![Effect::log(
        EventLevel::Info,
        "task.throttled",
        format!("no model fits the budget; retry at {}", retry_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        serde_json::json!({ "retry_at": retry_at, "reasons": decision.reasons, "prediction": decision.prediction }),
    )]
}

/// What [`on_starting`] settled for the attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct Start {
    /// `pq/<slug>` (or the branch of an earlier attempt).
    pub branch: String,
    /// Where the worktree goes (an earlier attempt's or the review's path is reused).
    pub worktree: PathBuf,
    pub attempt: u32,
    pub effects: Vec<Effect>,
}

/// The policy chose `model` for the task: it goes `starting` on that model,
/// and the branch, worktree path and attempt number of this try are settled
/// (not yet on the task: [`claim`] puts them there once the `starting` row is
/// written, so a daemon that dies mid-launch leaves the previous attempt
/// count behind). `resuming_with_prompt` says a review round or a relayed
/// answer is about to be resumed, which un-parks a review watch (`task
/// retry` of a task parked with its rounds used up). `root` is the worktree
/// root directory.
pub fn on_starting(
    task: &mut Task,
    model: &ModelTier,
    decision: &Decision,
    cfg: &Config,
    root: &Path,
    resuming_with_prompt: bool,
) -> Start {
    let attempt = task.attempts + 1;
    if resuming_with_prompt && let Some(watch) = task.review.as_mut() {
        watch.parked = false;
    }
    task.state = TaskState::Starting;
    task.model = Some(model.clone());
    let branch = task.branch.clone().unwrap_or_else(|| branch_name(&cfg.repo.branch_template, &task.slug(), &task.id.short()));
    let worktree = task
        .worktree_path
        .clone()
        .or_else(|| task.review.as_ref().and_then(|r| r.worktree_path.clone()))
        .map(PathBuf::from)
        .unwrap_or_else(|| worktree_dir(root, task));
    let effects = vec![Effect::log(
        EventLevel::Info,
        "task.starting",
        format!("starting attempt {attempt} with {model}"),
        serde_json::json!({ "model": model, "attempt": attempt, "reasons": decision.reasons, "prediction": decision.prediction }),
    )];
    Start { branch, worktree, attempt, effects }
}

/// The attempt is under way: its branch, worktree path and number go on
/// the task, to be persisted with whatever comes next (the launch, or the
/// crash that prevented it).
pub fn claim(task: &mut Task, start: &Start) {
    task.branch = Some(start.branch.clone());
    task.worktree_path = Some(start.worktree.display().to_string());
    task.attempts = start.attempt;
}

/// How to start the next attempt: `(session id, resume?, provider session id)`.
///
/// A crashed previous session — or, for a review round (`review`), the
/// session that armed the merge, whatever its state — is resumed when it ran
/// on the same provider and, for CLIs that generate their own ids, its id
/// was discovered; otherwise the attempt starts fresh with a new session id.
/// `transcript_present` says whether the crashed session left the transcript
/// its provider resumes from. A session that died before its first prompt
/// has nothing to resume: `--resume` would fail on every remaining attempt
/// ("No conversation found with session ID"), so it starts over instead.
pub fn resume_plan(
    previous: Option<&Session>,
    model: &ModelTier,
    transcript_present: bool,
    review: bool,
) -> (uuid::Uuid, bool, Option<String>) {
    match previous {
        Some(p)
            if (p.state == SessionState::Crashed || review) && p.model.provider() == model.provider() && transcript_present =>
        {
            if agent_for(model.provider()).accepts_session_id() {
                (p.id, true, None)
            } else if let Some(id) = &p.agent_session_id {
                (p.id, true, Some(id.clone()))
            } else {
                (uuid::Uuid::new_v4(), false, None)
            }
        }
        _ => (uuid::Uuid::new_v4(), false, None),
    }
}

/// What the next attempt is told, from [`resume_prompt`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResumePrompt {
    /// The review prompt and/or the relayed answer, joined (for the launch event).
    pub resume_prompt: Option<String>,
    /// Replaces the task prompt when the session is resumed (it knows the task).
    pub prompt_override: Option<String>,
    /// Goes into the task prompt's "previous error" slot when starting over.
    pub previous_error: Option<String>,
}

/// Compose what a resumed or restarted session is told. Resumed: only the
/// review prompt and/or the relayed answer. Started over (other provider,
/// no transcript): the full task prompt, with them as what to do first;
/// without either, the last error as before.
pub fn resume_prompt(
    review_prompt: Option<&str>,
    answer: Option<&str>,
    resume: bool,
    pr_url: Option<&str>,
    last_error: Option<&str>,
) -> ResumePrompt {
    let resume_prompt = match (review_prompt, answer) {
        (Some(r), Some(a)) => Some(format!("{r}\n\n{a}")),
        (Some(r), None) => Some(r.to_string()),
        (None, Some(a)) => Some(a.to_string()),
        (None, None) => None,
    };
    let (prompt_override, previous_error) = match &resume_prompt {
        Some(text) if resume => (Some(text.clone()), last_error.map(str::to_string)),
        Some(text) if review_prompt.is_some() => (
            None,
            Some(format!(
                "the pull request {} needs work; the previous session cannot be resumed. Start with: {text}",
                pr_url.unwrap_or("?")
            )),
        ),
        Some(text) => (None, Some(format!("the previous session asked a question and cannot be resumed. {text}"))),
        None => (None, last_error.map(str::to_string)),
    };
    ResumePrompt { resume_prompt, prompt_override, previous_error }
}

/// Facts about a session that just launched, for [`on_launched`].
#[derive(Debug, Clone, Copy)]
pub struct Launched<'a> {
    pub model: &'a ModelTier,
    pub attempt: u32,
    pub resume: bool,
    /// The review round being run, if any.
    pub relaunch: Option<&'a ReviewRelaunch>,
    pub review_prompt: Option<&'a str>,
    pub resume_prompt: Option<&'a str>,
    pub branch: &'a str,
    pub worktree: &'a Path,
    /// A relayed answer was handed to the session (it is forgotten now).
    pub answered: bool,
}

/// The session is up: the task runs. Tells the source issue: a review round
/// only comments (the PR already moved the issue); a later attempt after a
/// crash or timeout comments, since the first start already moved the issue
/// and a PR may have moved it since (unless a blocked state/label is
/// configured, in which case every attempt moves it, undoing a block); a
/// first attempt moves it to in progress.
pub fn on_launched(task: &mut Task, session: &Session, launched: &Launched<'_>, cfg: &Config, now: DateTime<Utc>) -> Vec<Effect> {
    let Launched { model, attempt, resume, relaunch, review_prompt, resume_prompt, branch, worktree, answered } = *launched;
    task.state = TaskState::Running;
    task.not_before = None;
    task.started_at = task.started_at.or(Some(now));
    let mut effects = Vec::new();
    if answered {
        effects.push(Effect::ForgetAnswer);
    }
    effects.push(Effect::log(
        EventLevel::Info,
        "session.launched",
        format!(
            "launched {model} session (attempt {attempt}{}{}) in window {}",
            if resume { ", resumed" } else { "" },
            relaunch.map(|r| format!(", review: {}", r.reason)).unwrap_or_default(),
            session.tmux_window
        ),
        serde_json::json!({
            "model": model,
            "attempt": attempt,
            "resume": resume,
            "review": relaunch,
            "prompt": resume_prompt,
            "answer": answered,
            "branch": branch,
            "worktree": worktree,
            "window": session.tmux_window,
            "pane": session.pane_id,
        }),
    ));
    let blocked_configured = match &task.source {
        TaskSource::GitHub { .. } => cfg.github.blocked_label.is_some(),
        _ => cfg.linear.blocked_state.as_deref().is_some_and(|s| !s.trim().is_empty()),
    };
    match (relaunch, review_prompt) {
        (Some(r), Some(prompt)) => {
            let detail = if r.detail.is_empty() { String::new() } else { format!(" ({})", r.detail) };
            effects.push(Effect::ProgressComment {
                body: format!("powerqueue resumed the session for PR #{}: {}{detail}. Prompt: `{prompt}`", r.pr_number, r.reason),
            });
        }
        _ if attempt > 1 && !blocked_configured => effects.push(Effect::ProgressComment {
            body: format!("powerqueue resumed attempt {attempt} on branch `{branch}` with model {model}."),
        }),
        _ => effects.push(Effect::Linear {
            target: LinearTarget::InProgress,
            comment: Some(format!("powerqueue started attempt {attempt} on branch `{branch}` with model {model}.")),
        }),
    }
    effects
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::budget::{AnchorSource, Ledger, Period, Prediction, TierLedger};
    use crate::domain::{Provider, ReviewWatch, SessionState};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn task(key: &str, state: TaskState, score: f64) -> Task {
        let mut t = Task::new(key, "t", TaskSource::Manual);
        t.state = state;
        t.score = score;
        t
    }

    fn session(task: &Task, state: SessionState, model: ModelTier, agent: Option<&str>) -> Session {
        Session {
            id: uuid::Uuid::new_v4(),
            task_id: task.id,
            attempt: 1,
            model,
            state,
            tmux_session: "pq".into(),
            tmux_window: "@1".into(),
            pane_id: Some("%1".into()),
            pid: None,
            transcript_path: None,
            exit_code: Some(1),
            started_at: now(),
            ended_at: None,
            last_activity_at: now(),
            error: None,
            agent_session_id: agent.map(str::to_string),
            waiting_since: None,
            waited_secs: 0,
        }
    }

    fn decision(model: Option<&str>) -> Decision {
        Decision {
            model: model.map(ModelTier::new),
            retry_at: None,
            prediction: Prediction { weighted_tokens: 1000.0, wall_secs: 60.0, confidence: 0.0, basis: "test".into() },
            reasons: vec!["test".into()],
        }
    }

    fn claude_ledger(budget: f64) -> Ledger {
        let mut ledger = Ledger::blank(
            Provider::Claude,
            now(),
            Period { start: now() - Duration::days(1), end: now() + Duration::days(6) },
            Period { start: now() - Duration::hours(5), end: now() },
        );
        ledger.tiers = vec![
            TierLedger { tier: ModelTier::opus(), period_budget: budget * 0.5, ..Default::default() },
            TierLedger { tier: ModelTier::sonnet(), period_budget: budget * 0.5, ..Default::default() },
        ];
        ledger.period_budget = budget;
        ledger.window_budget = budget;
        ledger.anchor_source = AnchorSource::Default;
        ledger
    }

    fn planner(tasks: Vec<Task>, slots: u32) -> LaunchPlanner {
        LaunchPlanner::new(Estimator::from_summaries(&[]), Ledgers::single(claude_ledger(1.0e9)), tasks, slots)
    }

    struct Fixed {
        cfg: Config,
        rules: PriorityRules,
        limits: RateLimitState,
    }

    impl Fixed {
        fn new() -> Self {
            Self { cfg: Config::default(), rules: PriorityRules::default(), limits: RateLimitState::default() }
        }

        fn ctx(&self) -> LaunchContext<'_> {
            LaunchContext { budget: &self.cfg.budget, rules: &self.rules, rate_limits: &self.limits }
        }
    }

    #[test]
    fn reserve_counts_against_tier_and_totals() {
        let mut ledger = claude_ledger(100.0);
        ledger.tiers.truncate(1);
        let mut ledgers = Ledgers::single(ledger);
        reserve(&mut ledgers, &ModelTier::opus(), 30.0);
        reserve(&mut ledgers, &ModelTier::haiku(), 5.0);
        reserve(&mut ledgers, &ModelTier::new("gpt-6-luna"), 7.0);
        let ledger = ledgers.get(Provider::Claude).unwrap();
        assert_eq!(ledger.tier(&ModelTier::opus()).period_weighted, 30.0);
        assert_eq!(ledger.tier(&ModelTier::opus()).window_weighted, 30.0);
        assert_eq!(ledger.total_period_weighted, 35.0);
        assert_eq!(ledger.total_window_weighted, 35.0, "another provider's model never lands in claude's ledger");
    }

    #[test]
    fn the_planner_hands_out_tasks_by_score_until_slots_run_out() {
        let tasks = vec![
            task("low", TaskState::Queued, 10.0),
            task("high", TaskState::Queued, 90.0),
            task("paused", TaskState::Paused, 100.0),
            task("mid", TaskState::Crashed, 50.0),
        ];
        let f = Fixed::new();
        let mut p = planner(tasks.clone(), 2);
        let first = p.next(f.ctx(), now()).expect("a candidate");
        assert_eq!(first.task.key, "high");
        assert!(first.decision.model.is_some(), "{:?}", first.decision.reasons);
        p.started(&f.cfg.budget, &first);
        let spent = p.ledgers().get(Provider::Claude).unwrap().total_period_weighted;
        assert!(spent > 0.0, "the predicted cost is reserved");
        let second = p.next(f.ctx(), now()).expect("a second candidate");
        assert_eq!(second.task.key, "mid");
        // Not started (launch failed): the slot stays free for the next one.
        let third = p.next(f.ctx(), now()).expect("the slot is still free");
        assert_eq!(third.task.key, "low");
        p.started(&f.cfg.budget, &third);
        assert!(p.next(f.ctx(), now()).is_none(), "no slot left");

        // The whole plan: pass order first, then what the pass would not
        // reach (here: a task waiting on its retry time).
        let mut waiting = task("later", TaskState::Throttled, 70.0);
        waiting.not_before = Some(now() + Duration::hours(1));
        let mut tasks = tasks;
        tasks.push(waiting);
        let all = planner(tasks, 10).plan_all(f.ctx(), now());
        assert_eq!(all.iter().map(|c| c.task.key.as_str()).collect::<Vec<_>>(), ["high", "mid", "low", "later"]);
        assert!(all.iter().all(|c| c.decision.model.is_some()));
        assert!(planner(Vec::new(), 3).next(f.ctx(), now()).is_none());
        assert!(
            planner(vec![task("x", TaskState::Queued, 1.0)], 0).next(f.ctx(), now()).is_none(),
            "no slot: nothing is handed out"
        );
    }

    #[test]
    fn a_throttled_decision_consumes_nothing() {
        let f = Fixed::new();
        let mut p = planner(vec![task("x", TaskState::Queued, 1.0)], 1);
        let throttled = Candidate { task: task("x", TaskState::Queued, 1.0), preferred: Vec::new(), decision: decision(None) };
        p.started(&f.cfg.budget, &throttled);
        assert!(p.next(f.ctx(), now()).is_some(), "the slot is still free");
        assert_eq!(p.ledgers().get(Provider::Claude).unwrap().total_period_weighted, 0.0);
    }

    #[test]
    fn the_planner_sees_rate_limits_marked_between_steps() {
        let mut f = Fixed::new();
        let mut p = planner(vec![task("a", TaskState::Queued, 2.0), task("b", TaskState::Queued, 1.0)], 2);
        let first = p.next(f.ctx(), now()).expect("a candidate");
        let model = first.decision.model.clone().expect("a model");
        // A failed start marks the provider; the next candidate must not get it.
        f.limits.mark_provider(&f.cfg.budget, model.provider(), now() + Duration::minutes(10));
        let second = p.next(f.ctx(), now()).expect("a second candidate");
        assert_ne!(second.decision.model.as_ref().map(|m| m.provider()), Some(model.provider()), "{:?}", second.decision);
    }

    #[test]
    fn throttling_sets_the_retry_time_and_logs_once() {
        let mut t = task("x", TaskState::Queued, 1.0);
        let mut d = decision(None);
        d.retry_at = Some(now() + Duration::minutes(7));
        let effects = on_throttled(&mut t, &d, now());
        assert!(
            matches!(&effects[..], [Effect::Log { kind, data, .. }] if kind == "task.throttled" && data["reasons"][0] == "test")
        );
        assert_eq!((t.state, t.not_before), (TaskState::Throttled, Some(now() + Duration::minutes(7))));
        d.retry_at = None;
        assert!(on_throttled(&mut t, &d, now()).is_empty(), "still throttled: quiet");
        assert_eq!(t.not_before, Some(now() + WINDOW_RECHECK));
    }

    #[test]
    fn starting_settles_branch_worktree_and_attempt() {
        let cfg = Config::default();
        let root = Path::new("/tmp/wt");
        let mut t = task("ENG-7", TaskState::Queued, 1.0);
        let start = on_starting(&mut t, &ModelTier::opus(), &decision(Some("opus")), &cfg, root, false);
        assert_eq!((t.state, t.model.clone()), (TaskState::Starting, Some(ModelTier::opus())));
        assert_eq!(start.attempt, 1);
        assert!(start.branch.contains("eng-7"), "{}", start.branch);
        assert!(start.worktree.starts_with(root));
        assert_eq!((t.attempts, t.branch.as_deref(), t.worktree_path.as_deref()), (0, None, None), "settled, not yet claimed");
        assert!(
            matches!(&start.effects[..], [Effect::Log { kind, message, .. }] if kind == "task.starting" && message == "starting attempt 1 with opus")
        );
        claim(&mut t, &start);
        assert_eq!(t.attempts, 1);
        assert_eq!(t.branch.as_deref(), Some(start.branch.as_str()));
        assert_eq!(t.worktree_path.as_deref(), Some(start.worktree.to_str().unwrap()));

        // A later attempt keeps its branch; a review round reuses the
        // remembered worktree path and un-parks the watch.
        let mut t = task("ENG-7", TaskState::Crashed, 1.0);
        t.attempts = 2;
        t.branch = Some("pq/keep".into());
        let mut watch = ReviewWatch::armed(now(), None);
        watch.worktree_path = Some("/elsewhere/eng-7".into());
        watch.parked = true;
        t.review = Some(watch);
        let start = on_starting(&mut t, &ModelTier::sonnet(), &decision(Some("sonnet")), &cfg, root, true);
        assert_eq!((start.branch.as_str(), start.attempt), ("pq/keep", 3));
        assert_eq!(start.worktree, PathBuf::from("/elsewhere/eng-7"));
        assert!(!t.review.unwrap().parked);
    }

    #[test]
    fn resume_plan_needs_same_provider_and_a_known_id() {
        let t = task("x", TaskState::Crashed, 1.0);
        let crashed = |model: ModelTier, agent: Option<&str>| session(&t, SessionState::Crashed, model, agent);
        let claude = crashed(ModelTier::opus(), None);
        assert_eq!(resume_plan(Some(&claude), &ModelTier::sonnet(), true, false), (claude.id, true, None));
        let (id, resume, _) = resume_plan(Some(&claude), &ModelTier::sonnet(), false, false);
        assert!(!resume && id != claude.id, "no transcript to resume from: start fresh");
        let codex = ModelTier::new("gpt-6-astra");
        let (id, resume, _) = resume_plan(Some(&claude), &codex, true, false);
        assert!(!resume && id != claude.id, "never resume across providers");
        let found = crashed(codex.clone(), Some("thread-1"));
        assert_eq!(resume_plan(Some(&found), &codex, true, false), (found.id, true, Some("thread-1".into())));
        assert!(!resume_plan(Some(&found), &codex, false, false).1);
        let unknown = crashed(codex.clone(), None);
        let (id, resume, _) = resume_plan(Some(&unknown), &codex, true, false);
        assert!(!resume && id != unknown.id, "no discovered id: start fresh");
        let mut exited = found.clone();
        exited.state = SessionState::Exited;
        assert!(!resume_plan(Some(&exited), &codex, true, false).1);
        assert!(!resume_plan(None, &codex, true, false).1);
        // A review round resumes the session that armed the merge, even though it exited.
        assert_eq!(resume_plan(Some(&exited), &codex, true, true), (exited.id, true, Some("thread-1".into())));
        assert!(!resume_plan(Some(&exited), &ModelTier::sonnet(), true, true).1, "still never across providers");
    }

    #[test]
    fn resume_prompts_depend_on_whether_the_session_is_resumed() {
        assert_eq!(
            resume_prompt(None, None, true, None, Some("err")),
            ResumePrompt { resume_prompt: None, prompt_override: None, previous_error: Some("err".into()) }
        );
        let p = resume_prompt(Some("fix CI"), Some("yes, nullable"), true, Some("u"), Some("err"));
        assert_eq!(p.resume_prompt.as_deref(), Some("fix CI\n\nyes, nullable"));
        assert_eq!(p.prompt_override.as_deref(), Some("fix CI\n\nyes, nullable"), "resumed: the prompt is the message");
        assert_eq!(p.previous_error.as_deref(), Some("err"));
        let p = resume_prompt(Some("fix CI"), None, false, Some("https://pr/1"), None);
        assert_eq!(p.prompt_override, None, "started over: the full task prompt");
        assert_eq!(
            p.previous_error.as_deref(),
            Some("the pull request https://pr/1 needs work; the previous session cannot be resumed. Start with: fix CI")
        );
        let p = resume_prompt(None, Some("use the users table"), false, None, Some("ignored"));
        assert_eq!(
            p.previous_error.as_deref(),
            Some("the previous session asked a question and cannot be resumed. use the users table")
        );
    }

    #[test]
    fn launched_runs_the_task_and_tells_the_issue_the_right_thing() {
        let cfg = Config::default();
        let mut t = task("ENG-7", TaskState::Starting, 1.0);
        let s = session(&t, SessionState::Launching, ModelTier::opus(), None);
        let base = Launched {
            model: &ModelTier::opus(),
            attempt: 1,
            resume: false,
            relaunch: None,
            review_prompt: None,
            resume_prompt: None,
            branch: "pq/eng-7",
            worktree: Path::new("/wt/eng-7"),
            answered: false,
        };
        let effects = on_launched(&mut t, &s, &base, &cfg, now());
        assert_eq!((t.state, t.started_at), (TaskState::Running, Some(now())));
        assert!(
            matches!(&effects[0], Effect::Log { kind, message, .. } if kind == "session.launched" && message.contains("in window @1"))
        );
        assert!(
            matches!(&effects[1], Effect::Linear { target: LinearTarget::InProgress, comment: Some(c) } if c.starts_with("powerqueue started attempt 1"))
        );

        // A later attempt only comments, and a relayed answer is forgotten.
        let effects = on_launched(&mut t, &s, &Launched { attempt: 2, resume: true, answered: true, ..base }, &cfg, now());
        assert!(matches!(&effects[0], Effect::ForgetAnswer));
        assert!(matches!(&effects[2], Effect::ProgressComment { body } if body.starts_with("powerqueue resumed attempt 2")));
        assert_eq!(t.started_at, Some(now()), "the first start is kept");

        // With a blocked state configured every attempt moves the issue (undoing a block).
        let mut blocked = Config::default();
        blocked.linear.blocked_state = Some("Blocked".into());
        let effects = on_launched(&mut t, &s, &Launched { attempt: 2, ..base }, &blocked, now());
        assert!(matches!(&effects[1], Effect::Linear { target: LinearTarget::InProgress, .. }));

        // A review round comments with the prompt.
        let relaunch = ReviewRelaunch { pr_number: 9, reason: "ci_failed".into(), detail: "lint".into(), requested_at: now() };
        let launched =
            Launched { relaunch: Some(&relaunch), review_prompt: Some("fix lint"), resume_prompt: Some("fix lint"), ..base };
        let effects = on_launched(&mut t, &s, &launched, &cfg, now());
        assert!(
            matches!(&effects[1], Effect::ProgressComment { body } if body == "powerqueue resumed the session for PR #9: ci_failed (lint). Prompt: `fix lint`")
        );
    }
}

#[cfg(test)]
mod properties {
    use std::collections::HashSet;

    use chrono::Duration;
    use proptest::prelude::*;

    use super::*;
    use crate::domain::Provider;
    use crate::strategies::*;

    /// Distinct tasks (the generator's ids are random, but make sure).
    fn candidates() -> impl Strategy<Value = Vec<Task>> {
        prop::collection::vec(task(), 0..8).prop_map(|mut tasks| {
            let mut seen = HashSet::new();
            tasks.retain(|t| seen.insert(t.id));
            tasks
        })
    }

    struct Fixed {
        cfg: Config,
        rules: PriorityRules,
        limits: RateLimitState,
    }

    impl Fixed {
        fn new() -> Self {
            Self { cfg: Config::default(), rules: PriorityRules::default(), limits: RateLimitState::default() }
        }
        fn ctx(&self) -> LaunchContext<'_> {
            LaunchContext { budget: &self.cfg.budget, rules: &self.rules, rate_limits: &self.limits }
        }
    }

    /// Run a whole pass with `next` + `started`, returning the candidates
    /// in the order they were handed out.
    fn run_pass(planner: &mut LaunchPlanner, f: &Fixed, now: DateTime<Utc>) -> Vec<Candidate> {
        let mut out = Vec::new();
        while let Some(c) = planner.next(f.ctx(), now) {
            planner.started(&f.cfg.budget, &c);
            out.push(c);
        }
        out
    }

    proptest! {
        /// Starting is two-phase: `on_starting` settles the attempt without
        /// touching the task's bookkeeping; `claim` puts it on the task.
        #[test]
        fn starting_settles_then_claims(
            task in task_among(&[TaskState::Queued, TaskState::Crashed, TaskState::Throttled]),
            model in claude_model(),
            decision in decision(),
            resuming in any::<bool>(),
        ) {
            let cfg = Config::default();
            let root = Path::new("/wt");
            let mut t = task.clone();
            let start = on_starting(&mut t, &model, &decision, &cfg, root, resuming);
            prop_assert_eq!((t.state, t.model.clone()), (TaskState::Starting, Some(model.clone())));
            prop_assert_eq!(start.attempt, task.attempts + 1);
            if let Some(branch) = &task.branch {
                prop_assert_eq!(&start.branch, branch);
            }
            let expected_wt = task.worktree_path.clone().or_else(|| task.review.as_ref().and_then(|r| r.worktree_path.clone()));
            if let Some(wt) = expected_wt {
                prop_assert_eq!(start.worktree.display().to_string(), wt);
            } else {
                prop_assert!(start.worktree.starts_with(root));
            }
            // Only state, model and the parking flag may differ.
            let mut normalised = t.clone();
            normalised.state = task.state;
            normalised.model = task.model.clone();
            if let (Some(w), Some(before)) = (normalised.review.as_mut(), task.review.as_ref()) {
                prop_assert!(!w.parked || !resuming, "resuming un-parks the watch");
                prop_assert!(w.parked == before.parked || resuming, "the parking flag only moves when resuming");
                w.parked = before.parked;
            }
            prop_assert_eq!(&normalised, &task);
            prop_assert!(matches!(&start.effects[..], [Effect::Log { kind, .. }] if kind == "task.starting"), "{:?}", start.effects);
            claim(&mut t, &start);
            prop_assert_eq!(t.attempts, task.attempts + 1);
            prop_assert_eq!(t.branch.as_deref(), Some(start.branch.as_str()));
            prop_assert_eq!(t.worktree_path.clone(), Some(start.worktree.display().to_string()));
        }

        /// Throttling sets the retry time from the decision and logs once.
        #[test]
        fn throttling_is_quiet_the_second_time(task in task(), decision in decision()) {
            let now = origin();
            let mut t = task.clone();
            let effects = on_throttled(&mut t, &decision, now);
            let retry_at = decision.retry_at.unwrap_or(now + WINDOW_RECHECK);
            prop_assert_eq!((t.state, t.not_before), (TaskState::Throttled, Some(retry_at)));
            prop_assert!(retry_at > now);
            prop_assert_eq!(effects.len(), usize::from(task.state != TaskState::Throttled));
            let again = t.clone();
            prop_assert!(on_throttled(&mut t, &decision, now).is_empty());
            prop_assert_eq!(&t, &again);
        }

        /// A pass hands out each schedulable task at most once, in
        /// `pick_next` order, uses at most `slots` starts, and reserves
        /// exactly the predicted cost on exactly one ledger per start.
        #[test]
        fn a_pass_hands_out_tasks_once_in_order(tasks in candidates(), slots in 0u32..=4, ledgers in ledgers()) {
            let now = origin();
            let f = Fixed::new();
            let mut p = LaunchPlanner::new(Estimator::from_summaries(&[]), ledgers.clone(), tasks.clone(), slots);
            let mut handed: Vec<TaskId> = Vec::new();
            let mut starts = 0u32;
            let mut before = ledgers.clone();
            let mut expected_order: Vec<TaskId> = Vec::new();
            let mut remaining = tasks.clone();
            while let Some(c) = p.next(f.ctx(), now) {
                prop_assert!(starts < slots, "handed out after the slots were used");
                prop_assert!(!handed.contains(&c.task.id), "handed out twice");
                handed.push(c.task.id);
                // pick_next over what was not considered yet.
                let pick = pick_next(&remaining, now).map(|t| t.id).expect("something schedulable");
                prop_assert_eq!(pick, c.task.id);
                expected_order.push(pick);
                remaining.retain(|t| t.id != c.task.id);
                prop_assert!(c.task.state.is_schedulable() && !c.task.is_waiting());
                prop_assert!(c.task.not_before.is_none_or(|nb| nb <= now));
                // A throttled candidate consumes nothing.
                if c.decision.model.is_none() {
                    let snapshot = p.ledgers().clone();
                    p.started(&f.cfg.budget, &c);
                    prop_assert_eq!(p.ledgers(), &snapshot);
                    continue;
                }
                p.started(&f.cfg.budget, &c);
                starts += 1;
                let model = c.decision.model.clone().expect("a model");
                let cost = c.weighted_cost(&f.cfg.budget);
                for (provider, after) in p.ledgers().iter() {
                    let was = before.get(provider).expect("same providers");
                    if provider == model.provider() {
                        prop_assert!((after.total_period_weighted - was.total_period_weighted - cost).abs() < 1e-6 * cost.max(1.0));
                        prop_assert!((after.total_window_weighted - was.total_window_weighted - cost).abs() < 1e-6 * cost.max(1.0));
                    } else {
                        prop_assert_eq!(after, was);
                    }
                }
                before = p.ledgers().clone();
            }
            prop_assert!(starts <= slots);
            prop_assert_eq!(&handed, &expected_order);
            // Nothing left unconsidered that could start when slots remain.
            if starts < slots {
                prop_assert!(pick_next(&remaining, now).is_none());
            }
        }

        /// `plan_all` lists every schedulable candidate exactly once, the
        /// pass first, and nothing else.
        #[test]
        fn plan_all_covers_every_schedulable_task_once(tasks in candidates(), slots in 0u32..=4, ledgers in ledgers()) {
            let now = origin();
            let f = Fixed::new();
            let planner = LaunchPlanner::new(Estimator::from_summaries(&[]), ledgers.clone(), tasks.clone(), slots);
            let mut pass = LaunchPlanner::new(Estimator::from_summaries(&[]), ledgers, tasks.clone(), slots);
            let pass_ids: Vec<TaskId> = run_pass(&mut pass, &f, now).iter().map(|c| c.task.id).collect();
            let all = planner.plan_all(f.ctx(), now);
            let ids: Vec<TaskId> = all.iter().map(|c| c.task.id).collect();
            prop_assert!(ids.starts_with(&pass_ids), "{ids:?} vs {pass_ids:?}");
            let unique: HashSet<TaskId> = ids.iter().copied().collect();
            prop_assert_eq!(unique.len(), ids.len(), "a task listed twice");
            let schedulable: HashSet<TaskId> = tasks.iter().filter(|t| t.state.is_schedulable()).map(|t| t.id).collect();
            prop_assert_eq!(unique, schedulable);
        }

        /// A provider marked rate-limited between two steps of a pass is not
        /// handed to the next candidate.
        #[test]
        fn rate_limits_marked_between_steps_are_seen(ledgers in ledgers(), provider in prop::sample::select(Provider::ALL.to_vec())) {
            let now = origin();
            let mut f = Fixed::new();
            f.cfg.budget.providers.codex.enabled = true;
            f.cfg.budget.providers.gemini.enabled = true;
            let tasks = vec![
                { let mut t = Task::new("ENG-a", "a", TaskSource::Manual); t.score = 2.0; t },
                { let mut t = Task::new("ENG-b", "b", TaskSource::Manual); t.score = 1.0; t },
            ];
            let mut p = LaunchPlanner::new(Estimator::from_summaries(&[]), ledgers, tasks, 2);
            let first = p.next(f.ctx(), now).expect("a candidate");
            let blocked = first.decision.model.as_ref().map_or(provider, |m| m.provider());
            f.limits.mark_provider(&f.cfg.budget, blocked, now + Duration::minutes(10));
            let second = p.next(f.ctx(), now).expect("a second candidate");
            prop_assert_ne!(second.decision.model.as_ref().map(|m| m.provider()), Some(blocked), "{:?}", second.decision.reasons);
        }

        /// A session is resumed only with a transcript, on the same provider,
        /// and only a crashed one (or, for a review round, whatever ended).
        #[test]
        fn resume_plan_never_crosses_providers(
            (task, session) in task_with_session(),
            with_previous in any::<bool>(),
            model in model(),
            transcript_present in any::<bool>(),
            review in any::<bool>(),
        ) {
            let _ = task;
            let previous = with_previous.then_some(&session);
            let (id, resume, agent_id) = resume_plan(previous, &model, transcript_present, review);
            if resume {
                let p = previous.expect("a previous session");
                prop_assert!(transcript_present);
                prop_assert_eq!(p.model.provider(), model.provider());
                prop_assert!(p.state == SessionState::Crashed || review);
                prop_assert_eq!(id, p.id);
                let needs_agent_id = !agent_for(model.provider()).accepts_session_id();
                prop_assert_eq!(agent_id.is_some(), needs_agent_id);
                if needs_agent_id {
                    prop_assert_eq!(agent_id, p.agent_session_id.clone());
                }
            } else {
                prop_assert!(previous.is_none_or(|p| p.id != id), "a fresh start gets a fresh id");
                prop_assert_eq!(agent_id, None);
            }
        }

        /// What a resumed or restarted session is told is composed totally
        /// and consistently with whether it is resumed.
        #[test]
        fn resume_prompt_is_consistent(
            review_prompt in prop::option::of(line()),
            answer in prop::option::of(line()),
            resume in any::<bool>(),
            pr_url in prop::option::of(pr_url()),
            last_error in prop::option::of(line()),
        ) {
            let p = resume_prompt(review_prompt.as_deref(), answer.as_deref(), resume, pr_url.as_deref(), last_error.as_deref());
            prop_assert_eq!(p.resume_prompt.is_some(), review_prompt.is_some() || answer.is_some());
            prop_assert_eq!(p.prompt_override.is_some(), resume && p.resume_prompt.is_some());
            if p.prompt_override.is_some() {
                prop_assert_eq!(&p.prompt_override, &p.resume_prompt);
            }
            if let Some(url) = &pr_url {
                let mentions = p.previous_error.as_deref().is_some_and(|e| e.contains(url.as_str()));
                prop_assert_eq!(mentions, !resume && review_prompt.is_some());
            }
            if review_prompt.is_none() && answer.is_none() {
                prop_assert_eq!(p.previous_error, last_error);
            }
            if let (Some(r), Some(a)) = (&review_prompt, &answer) {
                prop_assert_eq!(p.resume_prompt.clone(), Some(format!("{r}\n\n{a}")));
            }
        }

        /// A launched session runs the task and tells the issue exactly one
        /// thing: a move to in-progress on a first attempt (or whenever a
        /// blocked state is configured), else a comment.
        #[test]
        fn launched_tells_the_issue_one_thing(
            (task, session) in task_with_session_among(&[TaskState::Starting]),
            model in model(),
            attempt in 1u32..=5,
            resume in any::<bool>(),
            relaunch in prop::option::of(review_relaunch()),
            review_prompt in prop::option::of(line()),
            answered in any::<bool>(),
            blocked_state in prop::option::of(Just("Blocked".to_string())),
        ) {
            let now = origin();
            let mut cfg = Config::default();
            cfg.linear.blocked_state = blocked_state.clone();
            let launched = Launched {
                model: &model,
                attempt,
                resume,
                relaunch: relaunch.as_ref(),
                review_prompt: review_prompt.as_deref(),
                resume_prompt: review_prompt.as_deref(),
                branch: "pq/x",
                worktree: Path::new("/wt/x"),
                answered,
            };
            let mut t = task.clone();
            let effects = on_launched(&mut t, &session, &launched, &cfg, now);
            prop_assert_eq!((t.state, t.not_before), (TaskState::Running, None));
            prop_assert_eq!(t.started_at, task.started_at.or(Some(now)));
            prop_assert!(task.state.can_transition_to(t.state));
            let forgot = effects.iter().any(|e| matches!(e, Effect::ForgetAnswer));
            prop_assert_eq!(forgot, answered);
            let progress = effects.iter().filter(|e| matches!(e, Effect::ProgressComment { .. })).count();
            let moved = effects.iter().filter(|e| matches!(e, Effect::Linear { target: LinearTarget::InProgress, .. })).count();
            prop_assert_eq!(progress + moved, 1, "{:?}", effects);
            let round = relaunch.is_some() && review_prompt.is_some();
            let blocked_configured = blocked_state.is_some();
            prop_assert_eq!(moved == 1, !round && (attempt == 1 || blocked_configured));
            prop_assert!(effects.iter().any(|e| matches!(e, Effect::Log { kind, .. } if kind == "session.launched")), "{:?}", effects);
        }
    }
}
