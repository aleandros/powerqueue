//! Pure state-transition logic for the daemon.
//!
//! Every function here mutates the in-memory [`Task`] / [`Session`] it is
//! given and returns a list of [`Effect`]s describing what the daemon must do
//! in the outside world (log an event, clean up, talk to Linear, poke tmux).
//! Nothing here touches the store, tmux, git or the network, so the whole
//! decision table is unit-tested without a runtime.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Config, SchedulerConfig};
use crate::domain::{EventLevel, HookEvent, ModelTier, Provider, Session, SessionState, Task, TaskState};
use crate::session::{HookOutcome, SessionProbe};

/// What Claude is told when a session sits idle without a completion signal.
pub const NUDGE_TEXT: &str = "If the task is complete, run the completion command and print the done marker; otherwise continue.";

/// How many pane lines the daemon captures for crash reports.
pub const CRASH_TAIL_LINES: u32 = 40;

/// Issue lifecycle target. The name is retained for API compatibility; the daemon
/// maps it to Linear states or GitHub labels/closure according to the task source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinearTarget {
    InProgress,
    Done,
    Blocked,
}

/// A side effect requested by a transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    /// Persist a timeline event.
    Log { level: EventLevel, kind: String, message: String, data: serde_json::Value },
    /// Run [`crate::scheduler::cleanup_task`].
    Cleanup { succeeded: bool },
    /// Update the task's issue source and/or post a comment (best effort).
    /// The variant name is retained for compatibility with existing consumers.
    Linear { target: LinearTarget, comment: Option<String> },
    /// Type text into the session's pane.
    Nudge { text: String },
    /// Kill the session's tmux window.
    KillWindow,
    /// Record a rate-limit cooldown for a tier.
    RateLimit { tier: ModelTier, until: DateTime<Utc> },
    /// Record a cooldown for every model of a provider (account-wide limits).
    RateLimitProvider { provider: Provider, until: DateTime<Utc> },
    /// The task went `in_review`: kill the session's window and release the
    /// worktree, keeping the branch for a later review round.
    ReleaseForReview,
    /// Post a comment on the Linear issue without moving it (best effort).
    LinearComment { body: String },
    /// Delete the task's local branch (its PR was merged).
    DeleteBranch,
    /// Relay the agent's question to the Linear issue (best effort, once
    /// per session and text; see [`crate::scheduler::relay`]). `reason` is
    /// what `task block` or the blocked marker said. Unless `confirmed`
    /// (blocked marker, question), the daemon posts it only if the agent
    /// itself ran `task block` (`RelayState::agent_blocked`).
    Question { text: String, reason: Option<String>, confirmed: bool },
}

impl Effect {
    fn log(level: EventLevel, kind: &str, message: impl Into<String>, data: serde_json::Value) -> Self {
        Effect::Log { level, kind: kind.to_string(), message: message.into(), data }
    }
}

/// Inputs for [`on_probe`] besides the probe itself.
#[derive(Debug, Clone, Default)]
pub struct ProbeContext {
    pub now: DateTime<Utc>,
    /// The daemon already nudged this session once.
    pub nudged: bool,
    /// Last lines of the pane, captured before anything is killed.
    pub pane_tail: Option<String>,
    /// When the session's provider is usable again, if it is cooling down
    /// (rate-limit marks or an observed exhaustion).
    pub provider_cooldown_until: Option<DateTime<Utc>>,
}

/// Pane text (lower-case substrings) of an agent that hit its usage limit and
/// waits in-session until the reset, then continues by itself (Claude Code:
/// `Usage limit reached · continuing automatically at 3:45pm · esc to cancel`).
pub const WAITING_FOR_RESET_SIGNATURES: [&str; 2] =
    ["usage limit reached · continuing automatically", "continuing automatically at"];

/// Does the pane's tail show the agent waiting for its usage limit to reset?
/// Only the last few non-empty lines count, so an old message scrolled up
/// does not keep a hung session alive.
pub fn pane_waits_for_usage_reset(pane_tail: &str) -> bool {
    let lines: Vec<&str> = pane_tail.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let start = lines.len().saturating_sub(8);
    lines[start..].iter().any(|l| {
        let lower = l.to_lowercase();
        WAITING_FOR_RESET_SIGNATURES.iter().any(|s| lower.contains(s))
    })
}

/// Apply a hook outcome. `period_end` is the end of the session provider's
/// period and caps rate-limit cooldowns (the allowance resets there anyway).
pub fn on_hook_outcome(
    task: &mut Task,
    session: &mut Session,
    outcome: &HookOutcome,
    cfg: &Config,
    now: DateTime<Utc>,
    period_end: DateTime<Utc>,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    // Late activity/notifications must not resurrect finished or paused work.
    if !matches!(outcome, HookOutcome::Started { .. } | HookOutcome::Completed { .. } | HookOutcome::SessionEnded { .. })
        && (task.state.is_handed_off() || task.state == TaskState::Paused || !session.state.is_live())
    {
        return effects;
    }
    session.last_activity_at = now;
    match outcome {
        HookOutcome::Started { transcript_path, source } => {
            if transcript_path.is_some() {
                session.transcript_path = transcript_path.clone();
            }
            // A SessionStart drained after the daemon already saw this
            // session die (crash right after launch) must not revive the
            // task: nothing probes a dead session, so it would sit in
            // `running` forever instead of being relaunched. A resumed
            // attempt reuses the row as `launching`, which is live.
            let live = session.state.is_live();
            if live && !task.state.is_handed_off() && task.state != TaskState::Paused {
                session.state = SessionState::Running;
            }
            // Compacting the context is the agent's own housekeeping, not an
            // answer: a task waiting on a human keeps waiting (otherwise the
            // stale timer kills it and the relaunch asks the question again).
            let still_waiting = task.state == TaskState::NeedsAttention && source == "compact";
            if live
                && !still_waiting
                && matches!(task.state, TaskState::Starting | TaskState::Idle | TaskState::NeedsAttention | TaskState::Crashed)
            {
                task.state = TaskState::Running;
                task.last_error = None;
            }
            let message = if live {
                format!("agent session started ({source})")
            } else {
                format!("agent session started ({source}) after it was marked {}; task left {}", session.state, task.state)
            };
            effects.push(Effect::log(
                EventLevel::Info,
                "session.started",
                message,
                serde_json::json!({ "source": source, "transcript_path": transcript_path, "attempt": session.attempt, "late": !live }),
            ));
        }
        HookOutcome::Completed { summary } => {
            if task.state == TaskState::InReview {
                // `task complete --pr` already handed the task to the PR
                // watcher; the done marker only ends the session.
                effects.extend(end_review_session(session, "the agent printed the done marker", now));
                return effects;
            }
            if !session.state.is_live() && task.state == TaskState::Completed {
                effects.push(Effect::log(
                    EventLevel::Debug,
                    "hook.duplicate",
                    "completion already processed",
                    serde_json::json!({}),
                ));
                return effects;
            }
            let summary = summary.trim();
            if !summary.is_empty() {
                task.summary = Some(summary.to_string());
            }
            task.state = TaskState::Completed;
            task.completed_at = Some(now);
            task.last_error = None;
            session.state = SessionState::Exited;
            session.ended_at = Some(now);
            effects.push(Effect::log(
                EventLevel::Info,
                "task.completed",
                format!("task completed: {}", task.summary.as_deref().unwrap_or("(no summary)")),
                serde_json::json!({ "summary": task.summary, "attempt": session.attempt, "model": session.model }),
            ));
            effects.push(Effect::Cleanup { succeeded: true });
            effects.push(Effect::Linear {
                target: LinearTarget::Done,
                comment: Some(format!(
                    "powerqueue completed this task on branch `{}` (attempt {}, model {}).\n\n{}",
                    task.branch.as_deref().unwrap_or("?"),
                    session.attempt,
                    session.model,
                    task.summary.as_deref().unwrap_or("No summary was provided.")
                )),
            });
        }
        HookOutcome::Blocked { reason, message } => {
            task.state = TaskState::NeedsAttention;
            task.last_error = Some(reason.clone());
            session.state = SessionState::Idle;
            effects.push(Effect::log(
                EventLevel::Warn,
                "task.blocked",
                format!("the agent reports a blocker: {reason}"),
                serde_json::json!({ "reason": reason }),
            ));
            // The question itself is posted by `Effect::Question`.
            effects.push(Effect::Linear { target: LinearTarget::Blocked, comment: None });
            effects.extend(question_effect(message, Some(reason), true));
        }
        HookOutcome::TurnEnded { last_message } => {
            session.state = SessionState::Idle;
            if task.state == TaskState::NeedsAttention && !waiting_for_permission(task) {
                // Maybe `powerqueue task block` without the marker: the
                // turn's final message carries the question. The daemon
                // checks that the agent ran it (not a human, not the daemon).
                effects.extend(question_effect(last_message, task.last_error.as_deref(), false));
            }
            if matches!(task.state, TaskState::Running | TaskState::Starting)
                || (task.state == TaskState::NeedsAttention && waiting_for_permission(task))
            {
                task.state = TaskState::Idle;
                task.last_error = None;
            }
            effects.push(Effect::log(
                EventLevel::Debug,
                "session.turn_ended",
                "turn ended without a completion marker",
                serde_json::json!({ "preview": preview(last_message, 200) }),
            ));
        }
        HookOutcome::RateLimited { error_type, message } => {
            let provider = session.model.provider();
            let cooldown = Duration::minutes(cfg.budget.provider(provider).rate_limit_cooldown_mins.max(1) as i64);
            let until = (now + cooldown).min(period_end).max(now + Duration::minutes(1));
            task.state = TaskState::Throttled;
            task.not_before = Some(until);
            task.last_error = Some(format!("{error_type}: {message}"));
            // Subscription limits (5-hour window, weekly cap) are account-wide,
            // so a `rate_limit` pauses every model of the provider; `overloaded`
            // is specific to the model that reported it.
            let account_wide = error_type != "overloaded";
            let scope = if account_wide {
                effects.push(Effect::RateLimitProvider { provider, until });
                format!("every {provider} model")
            } else {
                effects.push(Effect::RateLimit { tier: session.model.clone(), until });
                format!("{provider} model {}", session.model)
            };
            effects.push(Effect::log(
                EventLevel::Warn,
                "budget.rate_limited",
                format!(
                    "{provider} ({}) reported {error_type}; cooling down {scope} until {}",
                    session.model,
                    until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                ),
                serde_json::json!({
                    "provider": provider,
                    "tier": session.model,
                    "error_type": error_type,
                    "message": message,
                    "until": until,
                    "account_wide": account_wide,
                }),
            ));
        }
        HookOutcome::TurnFailed { error_type, message } => {
            task.last_error = Some(format!("{error_type}: {message}"));
            let level = if error_type == "authentication_failed" { EventLevel::Error } else { EventLevel::Warn };
            if error_type == "authentication_failed" {
                task.state = TaskState::NeedsAttention;
            }
            effects.push(Effect::log(
                level,
                "session.turn_failed",
                format!("turn failed: {error_type}: {message}"),
                serde_json::json!({ "error_type": error_type, "message": message }),
            ));
        }
        HookOutcome::SessionEnded { reason } => {
            if task.state == TaskState::InReview {
                effects.extend(end_review_session(session, &format!("session ended ({reason})"), now));
            } else if task.state.is_terminal() || task.state == TaskState::Paused {
                session.state = SessionState::Exited;
                session.ended_at = Some(now);
                effects.push(Effect::log(
                    EventLevel::Info,
                    "session.exited",
                    format!("session ended ({reason})"),
                    serde_json::json!({ "reason": reason }),
                ));
            } else {
                effects.extend(on_crash(
                    task,
                    Some(session),
                    &format!("session ended unexpectedly ({reason})"),
                    None,
                    &cfg.scheduler,
                    now,
                    None,
                ));
            }
        }
        HookOutcome::Notification { kind, message } => match kind.as_str() {
            "permission_prompt" => {
                task.state = TaskState::NeedsAttention;
                task.last_error = Some(format!("{PERMISSION_PREFIX} {message}"));
                effects.push(Effect::log(
                    EventLevel::Warn,
                    "session.permission_prompt",
                    format!("the agent is waiting for a permission decision: {message}"),
                    serde_json::json!({ "message": message }),
                ));
            }
            "idle_prompt" => {
                session.state = SessionState::Idle;
                if task.state == TaskState::Running {
                    task.state = TaskState::Idle;
                }
                effects.push(Effect::log(
                    EventLevel::Debug,
                    "session.idle_prompt",
                    "the agent is waiting for input",
                    serde_json::json!({ "message": message }),
                ));
            }
            other => effects.push(Effect::log(
                EventLevel::Debug,
                "session.notification",
                format!("notification ({other}): {message}"),
                serde_json::json!({ "kind": other, "message": message }),
            )),
        },
        HookOutcome::Activity { event } => {
            if *event == HookEvent::UserPromptSubmit {
                effects.extend(resume_after_activity(task, session));
            } else if matches!(event, HookEvent::PostToolUse | HookEvent::PostToolUseFailure) {
                effects.extend(on_progress(task, session));
            }
            effects.push(Effect::log(
                EventLevel::Debug,
                &format!("hook.{}", event.as_str().to_ascii_lowercase()),
                "activity",
                serde_json::json!({}),
            ));
        }
    }
    effects
}

/// States in which a live session is not working: waiting on a human,
/// paused by one, or waiting for a usage limit to reset.
fn is_waiting(state: TaskState) -> bool {
    matches!(state, TaskState::NeedsAttention | TaskState::Paused | TaskState::Throttled)
}

/// Track how long the session is not working (`needs_attention`, paused,
/// throttled): a wait starts when the task is first seen waiting and its
/// length is added to `waited_secs` when it stops. The end of a wait counts as activity, so a
/// wait that ends without a hook (`task resume`) is not taken for a hang.
/// No I/O or failure is possible.
pub fn note_waiting(task: &Task, session: &mut Session, now: DateTime<Utc>) {
    match (is_waiting(task.state), session.waiting_since) {
        (true, None) => session.waiting_since = Some(now),
        (false, Some(since)) => {
            session.waited_secs += (now - since).num_seconds().max(0);
            session.waiting_since = None;
            session.last_activity_at = session.last_activity_at.max(now);
        }
        _ => {}
    }
}

/// True when the attempt exceeded `max_session_secs` of *working* time.
/// A waiting task (`needs_attention`, paused, throttled) never times out, and time spent
/// waiting does not count (see [`Session::working_time`]).
pub fn session_timed_out(task: &Task, session: &Session, cfg: &SchedulerConfig, now: DateTime<Utc>) -> bool {
    cfg.max_session_secs > 0
        && !task.state.is_handed_off()
        && !is_waiting(task.state)
        && session.working_time(now) > Duration::seconds(cfg.max_session_secs as i64)
}

/// Effects that release a live session of a task that went `in_review`
/// outside the hook path: its window, slot and worktree (branch kept).
pub fn release_for_review(task: &Task) -> Vec<Effect> {
    vec![
        Effect::log(
            EventLevel::Info,
            "session.released",
            format!("task is in review ({}); releasing its slot and worktree", task.pr_url.as_deref().unwrap_or("no PR")),
            serde_json::json!({ "pr": task.pr_url }),
        ),
        Effect::ReleaseForReview,
    ]
}

/// Resume idle tasks and clear permission attention after agent progress.
/// Explicit blockers still require a reply: finishing `task block` or writing
/// its explanation is not a resolution. No I/O or failure is possible.
pub fn on_progress(task: &mut Task, session: &mut Session) -> Vec<Effect> {
    if task.state == TaskState::NeedsAttention && !waiting_for_permission(task) {
        return Vec::new();
    }
    resume_after_activity(task, session)
}

/// [`Effect::Question`] for the last paragraph of `message` (or `reason`
/// when the message has nothing left); nothing when both are empty.
fn question_effect(message: &str, reason: Option<&str>, confirmed: bool) -> Option<Effect> {
    let text = crate::scheduler::relay::question_text(message, reason)?;
    Some(Effect::Question { text, reason: reason.map(str::to_string).filter(|r| !r.trim().is_empty()), confirmed })
}

/// `last_error` prefix of a task waiting at a permission prompt.
pub const PERMISSION_PREFIX: &str = "waiting for permission:";

fn waiting_for_permission(task: &Task) -> bool {
    task.last_error.as_deref().is_some_and(|reason| reason.starts_with(PERMISSION_PREFIX))
}

fn resume_after_activity(task: &mut Task, session: &mut Session) -> Vec<Effect> {
    if !session.state.is_live()
        || !matches!(task.state, TaskState::Starting | TaskState::Running | TaskState::Idle | TaskState::NeedsAttention)
    {
        return Vec::new();
    }
    let attention = task.state == TaskState::NeedsAttention;
    task.state = TaskState::Running;
    task.last_error = None;
    session.state = SessionState::Running;
    if attention {
        vec![Effect::log(
            EventLevel::Info,
            "task.attention_resolved",
            "agent activity resumed; attention cleared",
            serde_json::json!({}),
        )]
    } else {
        Vec::new()
    }
}

/// Apply a liveness probe for a live session.
pub fn on_probe(
    task: &mut Task,
    session: &mut Session,
    probe: &SessionProbe,
    cfg: &SchedulerConfig,
    ctx: &ProbeContext,
) -> Vec<Effect> {
    let now = ctx.now;
    let mut effects = Vec::new();
    if !session.state.is_live() {
        return effects;
    }
    if !probe.is_alive() {
        if task.state == TaskState::InReview {
            session.exit_code = probe.exit_status;
            return end_review_session(session, "pane gone", now);
        }
        if task.state.is_terminal() {
            // The task finished (`task complete`, a done marker) and the
            // pane went with it, before the hook phase saw a Stop. The
            // session stays live so `finalize_terminal` releases it with
            // cleanup and the source-issue update, as the hook path would.
            session.exit_code = probe.exit_status;
            effects.push(Effect::log(
                EventLevel::Debug,
                "session.pane_gone",
                format!("pane gone; task is {}; finalizing", task.state),
                serde_json::json!({ "exit_status": probe.exit_status, "pane_exists": probe.pane_exists }),
            ));
            return effects;
        }
        if task.state == TaskState::Paused {
            session.state = SessionState::Exited;
            session.ended_at = Some(now);
            session.exit_code = probe.exit_status;
            effects.push(Effect::log(
                EventLevel::Debug,
                "session.exited",
                format!("pane gone; task is {}", task.state),
                serde_json::json!({ "exit_status": probe.exit_status, "pane_exists": probe.pane_exists }),
            ));
            return effects;
        }
        let reason = match (probe.pane_exists, probe.exit_status) {
            (false, _) => "tmux pane disappeared".to_string(),
            (true, Some(code)) => format!("{} exited with status {code}", session.model.provider().display_name()),
            (true, None) => format!("{} exited", session.model.provider().display_name()),
        };
        return on_crash(task, Some(session), &reason, probe.exit_status, cfg, now, ctx.pane_tail.as_deref());
    }

    // Alive.
    if task.state == TaskState::Throttled && task.not_before.is_some_and(|t| t <= now) {
        task.state = TaskState::Running;
        task.not_before = None;
        session.state = SessionState::Running;
        // The wait was not silence: staleness counts from the end of the cooldown.
        session.last_activity_at = session.last_activity_at.max(now);
        effects.push(Effect::log(
            EventLevel::Info,
            "task.resumed",
            "rate-limit cooldown passed; session still alive",
            serde_json::json!({}),
        ));
        return effects;
    }

    note_waiting(task, session, now);
    let since_activity = now - session.last_activity_at;
    let running = matches!(task.state, TaskState::Running | TaskState::Starting);
    let stale = running && cfg.stale_session_secs > 0 && since_activity > Duration::seconds(cfg.stale_session_secs as i64);
    let worked = session.working_time(now);
    let timed_out = session_timed_out(task, session, cfg, now);

    // An agent waiting in-session for its usage limit to reset is not hung:
    // while its provider is on cooldown, park the task as throttled instead
    // of killing the session.
    if (stale || timed_out)
        && let Some(until) = ctx.provider_cooldown_until.filter(|u| *u > now)
        && ctx.pane_tail.as_deref().is_some_and(pane_waits_for_usage_reset)
    {
        if !(task.state == TaskState::Throttled && task.not_before == Some(until)) {
            task.state = TaskState::Throttled;
            task.not_before = Some(until);
            effects.push(Effect::log(
                EventLevel::Info,
                "session.waiting_for_reset",
                format!(
                    "{} is waiting for its usage limit to reset; not treated as hung until {}",
                    session.model.provider(),
                    until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                ),
                serde_json::json!({
                    "provider": session.model.provider(),
                    "until": until,
                    "idle_secs": since_activity.num_seconds(),
                }),
            ));
        }
        return effects;
    }

    if stale {
        let reason =
            format!("no activity for {}s (stale_session_secs = {})", since_activity.num_seconds(), cfg.stale_session_secs);
        effects.push(Effect::log(
            EventLevel::Warn,
            "session.stale",
            reason.clone(),
            serde_json::json!({ "idle_secs": since_activity.num_seconds() }),
        ));
        effects.push(Effect::KillWindow);
        effects.extend(on_crash(task, Some(session), &reason, None, cfg, now, ctx.pane_tail.as_deref()));
        return effects;
    }
    if timed_out {
        let reason = format!(
            "agent worked for {}s in this attempt (max_session_secs = {}; {}s spent waiting not counted)",
            worked.num_seconds(),
            cfg.max_session_secs,
            session.waited_secs
        );
        effects.push(Effect::log(
            EventLevel::Warn,
            "session.timeout",
            reason.clone(),
            serde_json::json!({
                "age_secs": (now - session.started_at).num_seconds(),
                "worked_secs": worked.num_seconds(),
                "waited_secs": session.waited_secs,
            }),
        ));
        effects.push(Effect::KillWindow);
        effects.extend(on_crash(task, Some(session), &reason, None, cfg, now, ctx.pane_tail.as_deref()));
        return effects;
    }

    if task.state == TaskState::Idle
        && cfg.idle_timeout_secs > 0
        && since_activity > Duration::seconds(cfg.idle_timeout_secs as i64)
    {
        if ctx.nudged {
            task.state = TaskState::NeedsAttention;
            task.last_error = Some(format!("idle for {}s after a nudge", since_activity.num_seconds()));
            effects.push(Effect::log(
                EventLevel::Warn,
                "task.needs_attention",
                "session stayed idle after a nudge; a human must attach",
                serde_json::json!({ "idle_secs": since_activity.num_seconds() }),
            ));
        } else {
            session.last_activity_at = now;
            effects.push(Effect::Nudge { text: NUDGE_TEXT.to_string() });
            effects.push(Effect::log(
                EventLevel::Info,
                "session.nudged",
                "session idle without a completion marker; nudged once",
                serde_json::json!({ "idle_secs": since_activity.num_seconds() }),
            ));
        }
    }
    effects
}

/// An attempt died (dead pane, stale session, setup failure). Either schedule
/// a retry with backoff or give up after `max_attempts`. `session` is `None`
/// when the failure happened before a session existed (worktree/setup).
pub fn on_crash(
    task: &mut Task,
    session: Option<&mut Session>,
    reason: &str,
    exit_status: Option<i32>,
    cfg: &SchedulerConfig,
    now: DateTime<Utc>,
    pane_tail: Option<&str>,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    let mut attempt = task.attempts;
    if let Some(s) = session {
        s.state = SessionState::Crashed;
        s.ended_at = Some(now);
        s.exit_code = exit_status;
        s.error = Some(reason.to_string());
        attempt = s.attempt.max(attempt);
    }
    task.last_error = Some(reason.to_string());
    let max = task.max_attempts.unwrap_or(cfg.max_attempts).max(1);
    // A review round gets its own `max_attempts`: attempts used before the
    // watcher relaunched the task do not count.
    let base = task.review_relaunch().and(task.review.as_ref()).map_or(0, |r| r.attempt_base);
    let used = attempt.saturating_sub(base);
    let data = serde_json::json!({ "reason": reason, "exit_status": exit_status, "attempt": attempt, "max_attempts": max, "pane_tail": pane_tail });
    if used >= max {
        task.state = TaskState::Failed;
        task.completed_at = Some(now);
        task.not_before = None;
        effects.push(Effect::log(
            EventLevel::Error,
            "task.failed",
            format!("giving up after {attempt} attempt(s): {reason}"),
            data,
        ));
        effects.push(Effect::Cleanup { succeeded: false });
        effects.push(Effect::Linear {
            target: LinearTarget::Blocked,
            comment: Some(format!(
                "powerqueue gave up after {attempt} attempt(s) on branch `{}`.\n\nLast error: {reason}",
                task.branch.as_deref().unwrap_or("?")
            )),
        });
    } else {
        let backoff = Duration::seconds(cfg.backoff_for_attempt(used) as i64);
        task.state = TaskState::Crashed;
        task.not_before = Some(now + backoff);
        effects.push(Effect::log(
            EventLevel::Warn,
            "session.crashed",
            format!("{reason}; retrying in {}s (attempt {used} of {max})", backoff.num_seconds()),
            data,
        ));
    }
    effects
}

/// The session of a task that went `in_review` ended (done marker, exit,
/// pane gone). The first time it is seen live, its resources are released
/// for the review; later signals only log.
fn end_review_session(session: &mut Session, why: &str, now: DateTime<Utc>) -> Vec<Effect> {
    if !session.state.is_live() {
        return vec![Effect::log(
            EventLevel::Debug,
            "hook.duplicate",
            "session already released for review",
            serde_json::json!({ "why": why }),
        )];
    }
    session.state = SessionState::Exited;
    session.ended_at = Some(now);
    vec![
        Effect::log(
            EventLevel::Info,
            "session.released",
            format!("{why}; task is in review, releasing its slot and worktree"),
            serde_json::json!({ "why": why }),
        ),
        Effect::ReleaseForReview,
    ]
}

/// Move a task between `queued` and `blocked` from its Linear dependencies
/// (see [`Task::is_waiting`]). Only `queued` / `throttled` tasks are
/// blocked: paused tasks stay paused, and a task that already ran (crashed,
/// live) is never pulled back — [`crate::scheduler::pick_next`] still
/// refuses to relaunch it while it waits. A `blocked` task whose blockers
/// cleared goes back to `queued`. Containers stay `blocked` until the daemon
/// closes them.
pub fn on_dependencies(task: &mut Task) -> Vec<Effect> {
    let waiting = task.is_waiting();
    let waiting_on: Vec<String> = task.waiting_on().into_iter().map(str::to_string).collect();
    match task.state {
        TaskState::Queued | TaskState::Throttled if waiting => {
            let previous = task.state;
            task.state = TaskState::Blocked;
            task.not_before = None;
            let message = if task.is_container() {
                format!(
                    "parent issue with {} sub-issue(s); never scheduled, closed once they are done (open: {})",
                    task.children.len(),
                    list_or_none(&waiting_on)
                )
            } else {
                format!("waiting on {}", waiting_on.join(", "))
            };
            vec![Effect::log(
                EventLevel::Info,
                "task.blocked",
                message,
                serde_json::json!({
                    "previous_state": previous.as_str(),
                    "waiting_on": waiting_on,
                    "container": task.is_container(),
                }),
            )]
        }
        TaskState::Blocked if !waiting => {
            task.state = TaskState::Queued;
            let cleared: Vec<&str> = task.blocked_by.iter().map(|b| b.key.as_str()).collect();
            vec![Effect::log(
                EventLevel::Info,
                "task.unblocked",
                format!("blockers cleared ({}); queued", list_or_none(&cleared)),
                serde_json::json!({ "blocked_by": cleared }),
            )]
        }
        _ => Vec::new(),
    }
}

fn list_or_none<S: AsRef<str>>(items: &[S]) -> String {
    if items.is_empty() { "none".to_string() } else { items.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(", ") }
}

/// First `max` characters of a message, single-line.
pub fn preview(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(max).collect();
    if flat.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskSource;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn period_end() -> DateTime<Utc> {
        now() + Duration::days(3)
    }

    fn cfg() -> Config {
        Config::default()
    }

    fn linked(key: &str, state_type: &str) -> crate::domain::LinkedIssue {
        crate::domain::LinkedIssue { key: key.into(), title: String::new(), state_type: state_type.into(), pr_merged: false }
    }

    #[test]
    fn dependencies_block_and_unblock_queued_tasks() {
        let mut t = task(TaskState::Queued, 0);
        t.blocked_by = vec![linked("ENG-0", "completed"), linked("ENG-2", "started")];
        let effects = on_dependencies(&mut t);
        assert_eq!(t.state, TaskState::Blocked);
        assert!(
            matches!(&effects[..], [Effect::Log { kind, message, .. }] if kind == "task.blocked" && message == "waiting on ENG-2"),
            "{effects:?}"
        );
        assert!(on_dependencies(&mut t).is_empty(), "no event while it keeps waiting");

        // A merged PR satisfies the blocker even before Linear says Done.
        t.blocked_by[1].pr_merged = true;
        let effects = on_dependencies(&mut t);
        assert_eq!(t.state, TaskState::Queued);
        assert!(matches!(&effects[..], [Effect::Log { kind, .. }] if kind == "task.unblocked"), "{effects:?}");

        // A canceled blocker counts as satisfied too.
        let mut t = task(TaskState::Throttled, 0);
        t.not_before = Some(now());
        t.blocked_by = vec![linked("ENG-0", "canceled")];
        assert!(on_dependencies(&mut t).is_empty());
        assert_eq!(t.state, TaskState::Throttled);
    }

    #[test]
    fn dependencies_never_touch_paused_crashed_or_live_tasks() {
        for state in [TaskState::Paused, TaskState::Crashed, TaskState::Running, TaskState::NeedsAttention] {
            let mut t = task(state, 1);
            t.blocked_by = vec![linked("ENG-0", "started")];
            assert!(on_dependencies(&mut t).is_empty());
            assert_eq!(t.state, state);
        }
    }

    #[test]
    fn containers_stay_blocked_even_when_children_are_done() {
        let mut t = task(TaskState::Queued, 0);
        t.children = vec![linked("ENG-2", "completed"), linked("ENG-3", "unstarted")];
        let effects = on_dependencies(&mut t);
        assert_eq!(t.state, TaskState::Blocked);
        assert!(
            matches!(&effects[..], [Effect::Log { message, .. }] if message.contains("2 sub-issue(s)") && message.contains("open: ENG-3")),
            "{effects:?}"
        );
        assert_eq!(t.waiting_on(), vec!["ENG-3"]);
        t.children[1].state_type = "completed".into();
        assert!(on_dependencies(&mut t).is_empty());
        assert_eq!(t.state, TaskState::Blocked, "the daemon closes containers; they never run");
    }

    fn task(state: TaskState, attempts: u32) -> Task {
        let mut t = Task::new("ENG-1", "t", TaskSource::Manual);
        t.state = state;
        t.attempts = attempts;
        t.branch = Some("pq/eng-1".into());
        t
    }

    fn session(state: SessionState, attempt: u32) -> Session {
        Session {
            id: uuid::Uuid::new_v4(),
            task_id: crate::domain::TaskId::new(),
            attempt,
            model: ModelTier::opus(),
            state,
            tmux_session: "powerqueue".into(),
            tmux_window: "@1".into(),
            pane_id: Some("%1".into()),
            pid: Some(42),
            transcript_path: None,
            exit_code: None,
            started_at: now() - Duration::minutes(10),
            ended_at: None,
            last_activity_at: now() - Duration::minutes(1),
            error: None,
            agent_session_id: None,
            waiting_since: None,
            waited_secs: 0,
        }
    }

    fn alive() -> SessionProbe {
        SessionProbe {
            pane_exists: true,
            pane_dead: false,
            exit_status: None,
            pane_pid: Some(42),
            current_command: Some("node".into()),
        }
    }

    fn dead(status: Option<i32>) -> SessionProbe {
        SessionProbe { pane_exists: true, pane_dead: true, exit_status: status, pane_pid: None, current_command: None }
    }

    #[test]
    fn a_session_handed_off_to_review_is_released_once() {
        use crate::session::HookOutcome;
        let mut t = task(TaskState::InReview, 1);
        let mut s = session(SessionState::Idle, 1);
        let done = HookOutcome::Completed { summary: "shipped".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &done, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::InReview, "the done marker does not complete a task in review");
        assert_eq!(s.state, SessionState::Exited);
        assert_eq!(kinds(&fx), vec!["session.released", "release_for_review"]);
        // The SessionEnd that follows (window killed) is no crash and releases nothing twice.
        let ended = HookOutcome::SessionEnded { reason: "other".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &ended, &cfg(), now(), period_end());
        assert_eq!((t.state, kinds(&fx)), (TaskState::InReview, vec!["hook.duplicate".to_string()]));
        // Late activity never revives it.
        let late = HookOutcome::TurnEnded { last_message: "x".into() };
        assert!(on_hook_outcome(&mut t, &mut s, &late, &cfg(), now(), period_end()).is_empty());

        // A dead pane of a task in review is released, not counted as a crash.
        let mut t = task(TaskState::InReview, 1);
        let mut s = session(SessionState::Running, 1);
        let fx = on_probe(&mut t, &mut s, &dead(Some(0)), &cfg().scheduler, &ProbeContext { now: now(), ..Default::default() });
        assert_eq!((t.state, s.state), (TaskState::InReview, SessionState::Exited));
        assert_eq!(kinds(&fx), vec!["session.released", "release_for_review"]);
        // An old live session of a task in review is not timed out.
        let mut t = task(TaskState::InReview, 1);
        let mut s = session(SessionState::Running, 1);
        s.started_at = now() - Duration::hours(10);
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ProbeContext { now: now(), ..Default::default() });
        assert!(fx.is_empty(), "{fx:?}");
    }

    #[test]
    fn crashes_in_a_review_round_count_from_the_round() {
        let sc = cfg().scheduler;
        let mut t = task(TaskState::Running, 3);
        let mut watch = crate::domain::ReviewWatch::armed(now(), None);
        watch.attempt_base = 2;
        watch.relaunch = Some(crate::domain::ReviewRelaunch {
            pr_number: 7,
            reason: "conflict".into(),
            detail: String::new(),
            requested_at: now(),
        });
        t.review = Some(watch);
        let mut s = session(SessionState::Running, 3);
        on_crash(&mut t, Some(&mut s), "boom", Some(1), &sc, now(), None);
        assert_eq!(t.state, TaskState::Crashed, "attempt 3 is the round's first; max_attempts = 3 is not used up");
        let mut s = session(SessionState::Running, 5);
        on_crash(&mut t, Some(&mut s), "boom", Some(1), &sc, now(), None);
        assert_eq!(t.state, TaskState::Failed, "third attempt of the round");
    }

    fn kinds(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .map(|e| match e {
                Effect::Log { kind, .. } => kind.clone(),
                Effect::Cleanup { succeeded } => format!("cleanup({succeeded})"),
                Effect::Linear { target, .. } => format!("linear({target:?})"),
                Effect::Nudge { .. } => "nudge".into(),
                Effect::KillWindow => "kill".into(),
                Effect::RateLimit { .. } => "ratelimit".into(),
                Effect::RateLimitProvider { provider, .. } => format!("ratelimit({provider})"),
                Effect::ReleaseForReview => "release_for_review".into(),
                Effect::LinearComment { .. } => "linear_comment".into(),
                Effect::DeleteBranch => "delete_branch".into(),
                Effect::Question { .. } => "question".into(),
            })
            .collect()
    }

    #[test]
    fn late_start_hook_never_revives_a_crashed_session() {
        // The pane died and the probe marked the attempt crashed before the
        // SessionStart hook was drained.
        let mut t = task(TaskState::Crashed, 1);
        t.not_before = Some(now() + Duration::seconds(1));
        let mut s = session(SessionState::Crashed, 1);
        let out = HookOutcome::Started { transcript_path: Some("/tmp/x.jsonl".into()), source: "startup".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::Crashed, "stays crashed so the daemon relaunches it");
        assert_eq!(s.state, SessionState::Crashed);
        assert_eq!(s.transcript_path.as_deref(), Some("/tmp/x.jsonl"), "the transcript is still worth knowing");
        assert!(
            matches!(&fx[..], [Effect::Log { kind, message, .. }] if kind == "session.started" && message.contains("after it was marked crashed")),
            "{fx:?}"
        );

        // The resumed attempt reuses the row as `launching`: its start does count.
        s.state = SessionState::Launching;
        t.state = TaskState::Starting;
        on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!((t.state, s.state), (TaskState::Running, SessionState::Running));
    }

    #[test]
    fn started_sets_running_and_transcript() {
        let mut t = task(TaskState::Starting, 1);
        let mut s = session(SessionState::Launching, 1);
        let out = HookOutcome::Started { transcript_path: Some("/tmp/x.jsonl".into()), source: "startup".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(s.state, SessionState::Running);
        assert_eq!(s.transcript_path.as_deref(), Some("/tmp/x.jsonl"));
        assert_eq!(s.last_activity_at, now());
        assert_eq!(kinds(&fx), vec!["session.started"]);

        let mut paused = task(TaskState::Paused, 1);
        on_hook_outcome(&mut paused, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(paused.state, TaskState::Paused, "a paused task is not resumed by a hook");

        // A fast `task complete` can beat draining SessionStart. We still
        // need its real transcript path to collect the final usage.
        let mut completed = task(TaskState::Completed, 1);
        let mut exited = session(SessionState::Exited, 1);
        on_hook_outcome(&mut completed, &mut exited, &out, &cfg(), now(), period_end());
        assert_eq!(completed.state, TaskState::Completed);
        assert_eq!(exited.state, SessionState::Exited);
        assert_eq!(exited.transcript_path.as_deref(), Some("/tmp/x.jsonl"));
    }

    #[test]
    fn completed_marks_task_and_requests_cleanup_and_linear() {
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        let out = HookOutcome::Completed { summary: "  shipped it  ".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::Completed);
        assert_eq!(t.summary.as_deref(), Some("shipped it"));
        assert_eq!(t.completed_at, Some(now()));
        assert_eq!(s.state, SessionState::Exited);
        assert_eq!(kinds(&fx), vec!["task.completed", "cleanup(true)", "linear(Done)"]);
        match &fx[2] {
            Effect::Linear { comment: Some(c), .. } => assert!(c.contains("pq/eng-1") && c.contains("shipped it"), "{c}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn completed_twice_is_a_no_op() {
        let mut t = task(TaskState::Completed, 1);
        let mut s = session(SessionState::Exited, 1);
        let out = HookOutcome::Completed { summary: "again".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(kinds(&fx), vec!["hook.duplicate"]);
        assert_eq!(t.summary, None);
    }

    #[test]
    fn blocked_needs_attention_and_notifies_linear() {
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        let out = HookOutcome::Blocked {
            reason: "need creds".into(),
            message: "Tried staging.\n\nWhere are the staging creds?\n[[POWERQUEUE:BLOCKED]] need creds".into(),
        };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::NeedsAttention);
        assert_eq!(t.last_error.as_deref(), Some("need creds"));
        assert_eq!(s.state, SessionState::Idle);
        assert_eq!(kinds(&fx), vec!["task.blocked", "linear(Blocked)", "question"]);
        assert_eq!(fx[1], Effect::Linear { target: LinearTarget::Blocked, comment: None }, "the question is the comment");
        assert_eq!(
            fx[2],
            Effect::Question {
                text: "Where are the staging creds?\nneed creds".into(),
                reason: Some("need creds".into()),
                confirmed: true
            }
        );
    }

    #[test]
    fn turn_ended_after_task_block_relays_the_final_message() {
        let mut t = task(TaskState::NeedsAttention, 1);
        t.last_error = Some("which schema?".into());
        let mut s = session(SessionState::Running, 1);
        let out = HookOutcome::TurnEnded { last_message: "I ran `powerqueue task block`.\n\nShould I use schema A or B".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::NeedsAttention, "still waiting for the reply");
        assert!(fx.contains(&Effect::Question {
            text: "Should I use schema A or B".into(),
            reason: Some("which schema?".into()),
            confirmed: false
        }));

        // A permission prompt is no question.
        let mut t = task(TaskState::NeedsAttention, 1);
        t.last_error = Some("waiting for permission: Bash".into());
        let mut s = session(SessionState::Running, 1);
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert!(!kinds(&fx).contains(&"question".to_string()));
        // A plain turn end of a running task neither.
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert!(!kinds(&fx).contains(&"question".to_string()));
    }

    #[test]
    fn turn_ended_marks_idle_but_keeps_paused() {
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        let out = HookOutcome::TurnEnded { last_message: "working on it\n\nmore".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::Idle);
        assert_eq!(s.state, SessionState::Idle);
        assert_eq!(kinds(&fx), vec!["session.turn_ended"]);
        let mut p = task(TaskState::Paused, 1);
        on_hook_outcome(&mut p, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(p.state, TaskState::Paused);
    }

    #[test]
    fn rate_limited_throttles_until_cooldown_capped_by_period_end() {
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        let out = HookOutcome::RateLimited { error_type: "rate_limit".into(), message: "slow down".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::Throttled);
        assert_eq!(t.not_before, Some(now() + Duration::minutes(30)));
        assert_eq!(s.state, SessionState::Running, "the session stays alive; Claude retries by itself");
        assert_eq!(
            fx[0],
            Effect::RateLimitProvider { provider: Provider::Claude, until: now() + Duration::minutes(30) },
            "rate_limit is account-wide"
        );
        assert_eq!(kinds(&fx), vec!["ratelimit(claude)", "budget.rate_limited"]);
        match &fx[1] {
            Effect::Log { message, data, .. } => {
                assert!(message.starts_with("claude (opus) reported rate_limit; cooling down every claude model"), "{message}");
                assert_eq!(data["provider"], "claude");
            }
            other => panic!("{other:?}"),
        }

        let mut t = task(TaskState::Running, 1);
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), now() + Duration::minutes(5));
        assert_eq!(t.not_before, Some(now() + Duration::minutes(5)), "capped at the period end");
        assert!(matches!(fx[0], Effect::RateLimitProvider { until, .. } if until == now() + Duration::minutes(5)));

        let mut t = task(TaskState::Running, 1);
        let out = HookOutcome::RateLimited { error_type: "overloaded".into(), message: "busy".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(fx[0], Effect::RateLimit { tier: ModelTier::opus(), until: now() + Duration::minutes(30) });
        assert_eq!(kinds(&fx), vec!["ratelimit", "budget.rate_limited"], "overloaded only affects the reporting tier");
    }

    #[test]
    fn rate_limited_uses_the_sessions_provider() {
        let mut c = cfg();
        c.budget.providers.codex.rate_limit_cooldown_mins = 45;
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        s.model = ModelTier::new("gpt-6-astra");
        let out = HookOutcome::RateLimited { error_type: "rate_limit".into(), message: "usage limit".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &c, now(), period_end());
        assert_eq!(t.not_before, Some(now() + Duration::minutes(45)), "codex's own cooldown");
        assert_eq!(fx[0], Effect::RateLimitProvider { provider: Provider::Codex, until: now() + Duration::minutes(45) });
        assert_eq!(kinds(&fx), vec!["ratelimit(codex)", "budget.rate_limited"]);
    }

    const WAITING_PANE: &str =
        "⏺ Working on it\n\n  Usage limit reached · continuing automatically at 3:45pm · esc to cancel\n\n> \n";

    #[test]
    fn waiting_pane_detection() {
        assert!(pane_waits_for_usage_reset(WAITING_PANE));
        assert!(pane_waits_for_usage_reset("Usage limit reset · continuing automatically at 4pm"));
        assert!(!pane_waits_for_usage_reset("Usage limit reset · continuing automatically"));
        assert!(!pane_waits_for_usage_reset("all good\n> "));
        let scrolled = format!("{WAITING_PANE}{}", "more output\n".repeat(10));
        assert!(!pane_waits_for_usage_reset(&scrolled), "only the last lines count");
    }

    #[test]
    fn waiting_for_usage_reset_is_not_stale_while_provider_cools_down() {
        let until = now() + Duration::minutes(40);
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        s.last_activity_at = now() - Duration::hours(1);
        let ctx = ProbeContext {
            now: now(),
            nudged: false,
            pane_tail: Some(WAITING_PANE.into()),
            provider_cooldown_until: Some(until),
        };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(kinds(&fx), vec!["session.waiting_for_reset"]);
        assert_eq!(t.state, TaskState::Throttled);
        assert_eq!(t.not_before, Some(until));
        assert_eq!(s.state, SessionState::Running, "the session is left alone");
        // Re-probing while still waiting changes nothing (no duplicate events).
        assert!(on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx).is_empty());

        // The cooldown passes: the task resumes and staleness counts from now.
        let later = ProbeContext { now: until + Duration::seconds(1), ..ctx.clone() };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &later);
        assert_eq!(kinds(&fx), vec!["task.resumed"]);
        assert_eq!(s.last_activity_at, until + Duration::seconds(1));

        // Not on cooldown: the same pane is stale as before.
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        s.last_activity_at = now() - Duration::hours(1);
        let ctx = ProbeContext { provider_cooldown_until: None, ..ctx.clone() };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(kinds(&fx), vec!["session.stale", "kill", "session.crashed"]);

        // On cooldown but the pane shows something else: stale.
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        s.last_activity_at = now() - Duration::hours(1);
        let ctx = ProbeContext { pane_tail: Some("$ ".into()), provider_cooldown_until: Some(until), ..ctx };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(kinds(&fx), vec!["session.stale", "kill", "session.crashed"]);
    }

    #[test]
    fn waiting_for_usage_reset_also_defers_the_session_timeout() {
        let until = now() + Duration::hours(2);
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        s.started_at = now() - Duration::hours(5);
        let ctx = ProbeContext {
            now: now(),
            nudged: false,
            pane_tail: Some(WAITING_PANE.into()),
            provider_cooldown_until: Some(until),
        };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(kinds(&fx), vec!["session.waiting_for_reset"]);
        assert_eq!(t.state, TaskState::Throttled);
    }

    #[test]
    fn turn_failed_logs_and_auth_failure_needs_attention() {
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        let out = HookOutcome::TurnFailed { error_type: "server_error".into(), message: "500".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(kinds(&fx), vec!["session.turn_failed"]);
        let out = HookOutcome::TurnFailed { error_type: "authentication_failed".into(), message: "login".into() };
        on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::NeedsAttention);
    }

    #[test]
    fn session_ended_after_completion_exits_otherwise_crashes() {
        let mut t = task(TaskState::Completed, 1);
        let mut s = session(SessionState::Running, 1);
        let out = HookOutcome::SessionEnded { reason: "prompt_input_exit".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(s.state, SessionState::Exited);
        assert_eq!(kinds(&fx), vec!["session.exited"]);

        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::Crashed);
        assert_eq!(s.state, SessionState::Crashed);
        assert_eq!(t.not_before, Some(now() + Duration::seconds(30)));
        assert_eq!(kinds(&fx), vec!["session.crashed"]);
    }

    #[test]
    fn answering_or_finishing_a_tool_clears_attention_and_error() {
        for event in [HookEvent::UserPromptSubmit, HookEvent::PostToolUse, HookEvent::PostToolUseFailure] {
            for state in [SessionState::Idle, SessionState::Running] {
                let mut t = task(TaskState::NeedsAttention, 1);
                t.last_error = Some("waiting for permission: Bash".into());
                let mut s = session(state, 1);
                let out = HookOutcome::Activity { event };
                let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
                assert_eq!(t.state, TaskState::Running, "{event:?} / {state:?}");
                assert_eq!(s.state, SessionState::Running);
                assert_eq!(t.last_error, None);
                assert!(kinds(&fx).contains(&"task.attention_resolved".to_string()));
            }
        }
    }

    #[test]
    fn housekeeping_and_late_events_do_not_resume_tasks() {
        let mut t = task(TaskState::NeedsAttention, 1);
        let mut s = session(SessionState::Idle, 1);
        let out = HookOutcome::Activity { event: HookEvent::PreCompact };
        on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::NeedsAttention);
        for state in [TaskState::Paused, TaskState::Completed, TaskState::Cancelled, TaskState::Failed] {
            let mut t = task(state, 1);
            for out in [
                HookOutcome::Activity { event: HookEvent::PostToolUse },
                HookOutcome::Notification { kind: "permission_prompt".into(), message: "old prompt".into() },
            ] {
                on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
                assert_eq!(t.state, state);
            }
        }
        let mut t = task(TaskState::NeedsAttention, 1);
        let mut s = session(SessionState::Exited, 1);
        assert!(on_progress(&mut t, &mut s).is_empty());
        assert_eq!(t.state, TaskState::NeedsAttention);
    }

    #[test]
    fn a_new_unblocked_turn_end_clears_old_attention() {
        let mut t = task(TaskState::NeedsAttention, 1);
        t.last_error = Some("waiting for permission: Bash".into());
        let mut s = session(SessionState::Running, 1);
        let out = HookOutcome::TurnEnded { last_message: "Migration created.".into() };
        on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::Idle);
        assert_eq!(t.last_error, None);
    }

    #[test]
    fn explicit_blocker_survives_tool_completion_until_a_reply() {
        let mut t = task(TaskState::NeedsAttention, 1);
        t.last_error = Some("need staging credentials".into());
        let mut s = session(SessionState::Running, 1);
        for out in [
            HookOutcome::Activity { event: HookEvent::PostToolUse },
            HookOutcome::TurnEnded { last_message: "Waiting for credentials.".into() },
        ] {
            on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
            assert_eq!(t.state, TaskState::NeedsAttention);
        }
        assert!(on_progress(&mut t, &mut s).is_empty());
        let reply = HookOutcome::Activity { event: HookEvent::UserPromptSubmit };
        on_hook_outcome(&mut t, &mut s, &reply, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(t.last_error, None);
    }

    #[test]
    fn notifications() {
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        let out = HookOutcome::Notification { kind: "permission_prompt".into(), message: "Bash wants to rm".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::NeedsAttention);
        assert_eq!(kinds(&fx), vec!["session.permission_prompt"]);

        let mut t = task(TaskState::Running, 1);
        let out = HookOutcome::Notification { kind: "idle_prompt".into(), message: "".into() };
        on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::Idle);
        assert_eq!(s.state, SessionState::Idle);

        let out = HookOutcome::Activity { event: HookEvent::UserPromptSubmit };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::Running, "a new prompt wakes an idle session");
        assert_eq!(kinds(&fx), vec!["hook.userpromptsubmit"]);
    }

    #[test]
    fn dead_pane_crashes_with_backoff_and_captures_tail() {
        let mut t = task(TaskState::Running, 2);
        let mut s = session(SessionState::Running, 2);
        let ctx = ProbeContext { now: now(), nudged: false, pane_tail: Some("boom".into()), provider_cooldown_until: None };
        let fx = on_probe(&mut t, &mut s, &dead(Some(1)), &cfg().scheduler, &ctx);
        assert_eq!(t.state, TaskState::Crashed);
        assert_eq!(t.not_before, Some(now() + Duration::seconds(120)), "second attempt uses the second backoff");
        assert_eq!(t.last_error.as_deref(), Some("Claude Code exited with status 1"));
        assert_eq!(s.state, SessionState::Crashed);
        assert_eq!(s.exit_code, Some(1));
        match &fx[0] {
            Effect::Log { kind, data, .. } => {
                assert_eq!(kind, "session.crashed");
                assert_eq!(data["pane_tail"], "boom");
                assert_eq!(data["exit_status"], 1);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dead_pane_after_max_attempts_fails_and_cleans_up() {
        let mut t = task(TaskState::Running, 3);
        let mut s = session(SessionState::Running, 3);
        let ctx = ProbeContext { now: now(), ..Default::default() };
        let fx = on_probe(&mut t, &mut s, &dead(None), &cfg().scheduler, &ctx);
        assert_eq!(t.state, TaskState::Failed);
        assert_eq!(t.not_before, None);
        assert_eq!(kinds(&fx), vec!["task.failed", "cleanup(false)", "linear(Blocked)"]);

        let mut t = task(TaskState::Running, 1);
        t.max_attempts = Some(1);
        let mut s = session(SessionState::Running, 1);
        on_probe(&mut t, &mut s, &dead(None), &cfg().scheduler, &ctx);
        assert_eq!(t.state, TaskState::Failed, "per-task max_attempts wins");
    }

    #[test]
    fn dead_pane_for_paused_task_just_exits() {
        let mut t = task(TaskState::Paused, 1);
        let mut s = session(SessionState::Running, 1);
        let ctx = ProbeContext { now: now(), ..Default::default() };
        let fx = on_probe(&mut t, &mut s, &dead(Some(0)), &cfg().scheduler, &ctx);
        assert_eq!(t.state, TaskState::Paused);
        assert_eq!(s.state, SessionState::Exited);
        assert_eq!(kinds(&fx), vec!["session.exited"]);
    }

    /// A finished task whose pane died before the hook phase saw a Stop
    /// keeps its session live: `finalize_terminal` releases it with cleanup
    /// and the source-issue update, which a plain exit would skip.
    #[test]
    fn dead_pane_for_finished_task_is_left_to_finalize() {
        for state in [TaskState::Completed, TaskState::Cancelled, TaskState::Failed] {
            let mut t = task(state, 1);
            let mut s = session(SessionState::Running, 1);
            let ctx = ProbeContext { now: now(), ..Default::default() };
            let fx = on_probe(&mut t, &mut s, &dead(Some(0)), &cfg().scheduler, &ctx);
            assert_eq!(t.state, state);
            assert!(s.state.is_live(), "{state}: left for finalize_terminal");
            assert_eq!(s.exit_code, Some(0));
            assert_eq!(kinds(&fx), vec!["session.pane_gone"]);
        }
    }

    #[test]
    fn stale_session_is_killed_and_treated_as_crash() {
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        s.last_activity_at = now() - Duration::hours(1);
        let ctx = ProbeContext { now: now(), ..Default::default() };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(kinds(&fx), vec!["session.stale", "kill", "session.crashed"]);
        assert_eq!(t.state, TaskState::Crashed);

        let mut idle = task(TaskState::Idle, 1);
        let mut s = session(SessionState::Idle, 1);
        s.last_activity_at = now() - Duration::hours(1);
        let fx = on_probe(&mut idle, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert!(
            !kinds(&fx).contains(&"session.stale".to_string()),
            "idle sessions are governed by the idle timeout, not staleness"
        );
    }

    #[test]
    fn max_session_secs_is_enforced() {
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Running, 1);
        s.started_at = now() - Duration::hours(5);
        let ctx = ProbeContext { now: now(), ..Default::default() };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(kinds(&fx), vec!["session.timeout", "kill", "session.crashed"]);
    }

    #[test]
    fn waiting_on_a_human_never_times_out_and_does_not_count() {
        // AVS-1618 / AVS-1749: the agent asked a question, the human took
        // hours to answer, and max_session_secs killed the waiting session.
        let mut t = task(TaskState::NeedsAttention, 1);
        let mut s = session(SessionState::Idle, 1);
        s.started_at = now() - Duration::hours(5);
        s.last_activity_at = now() - Duration::hours(5);
        let asked = now() - Duration::hours(4) - Duration::minutes(50);
        let ctx = ProbeContext { now: asked, ..Default::default() };
        on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(s.waiting_since, Some(asked));

        let ctx = ProbeContext { now: now(), ..Default::default() };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert!(fx.is_empty(), "a task waiting on a human is not timed out: {fx:?}");
        assert_eq!(t.state, TaskState::NeedsAttention);

        // The human answers: the agent worked 10 minutes, not 5 hours.
        t.state = TaskState::Running;
        s.state = SessionState::Running;
        s.last_activity_at = now();
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert!(fx.is_empty(), "the wait does not count towards max_session_secs: {fx:?}");
        assert_eq!(s.waiting_since, None);
        assert_eq!(s.waited_secs, (Duration::hours(4) + Duration::minutes(50)).num_seconds());
        assert_eq!(s.working_time(now()), Duration::minutes(10));

        // Working time still runs out eventually.
        let later = now() + Duration::hours(4);
        s.last_activity_at = later;
        let ctx = ProbeContext { now: later, ..Default::default() };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(kinds(&fx), vec!["session.timeout", "kill", "session.crashed"]);
    }

    #[test]
    fn paused_sessions_do_not_time_out() {
        let mut t = task(TaskState::Paused, 1);
        let mut s = session(SessionState::Idle, 1);
        s.started_at = now() - Duration::hours(5);
        let ctx = ProbeContext { now: now(), ..Default::default() };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert!(fx.is_empty(), "{fx:?}");
        assert_eq!(s.waiting_since, Some(now()));
    }

    #[test]
    fn a_wait_ended_without_a_hook_is_not_taken_for_a_hang() {
        // `task resume` of a needs_attention task: no hook refreshes the
        // activity clock, so the stale check must not see hours of silence.
        let mut t = task(TaskState::NeedsAttention, 1);
        let mut s = session(SessionState::Idle, 1);
        s.last_activity_at = now() - Duration::hours(3);
        let asked = now() - Duration::hours(3);
        let ctx = ProbeContext { now: asked, ..Default::default() };
        on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);

        t.state = TaskState::Running;
        s.state = SessionState::Running;
        let ctx = ProbeContext { now: now(), ..Default::default() };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert!(fx.is_empty(), "{fx:?}");
        assert_eq!(s.last_activity_at, now());
        assert_eq!(s.waited_secs, Duration::hours(3).num_seconds());
    }

    #[test]
    fn compaction_does_not_clear_a_pending_question() {
        // AVS-1618: a compaction while the question was open moved the task
        // back to running; 30 minutes later the stale timer killed it.
        let mut t = task(TaskState::NeedsAttention, 1);
        t.last_error = Some("which option?".into());
        let mut s = session(SessionState::Idle, 1);
        let out = HookOutcome::Started { transcript_path: None, source: "compact".into() };
        on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::NeedsAttention);
        assert_eq!(t.last_error.as_deref(), Some("which option?"));

        let later = now() + Duration::hours(1);
        let ctx = ProbeContext { now: later, ..Default::default() };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert!(!kinds(&fx).iter().any(|k| k == "session.stale"), "{fx:?}");
    }

    #[test]
    fn idle_session_is_nudged_once_then_needs_attention() {
        let mut t = task(TaskState::Idle, 1);
        let mut s = session(SessionState::Idle, 1);
        s.last_activity_at = now() - Duration::minutes(11);
        let ctx = ProbeContext { now: now(), nudged: false, pane_tail: None, provider_cooldown_until: None };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(kinds(&fx), vec!["nudge", "session.nudged"]);
        assert_eq!(fx[0], Effect::Nudge { text: NUDGE_TEXT.to_string() });
        assert_eq!(s.last_activity_at, now(), "the second timeout counts from the nudge");
        assert_eq!(t.state, TaskState::Idle);

        s.last_activity_at = now() - Duration::minutes(11);
        let ctx = ProbeContext { now: now(), nudged: true, pane_tail: None, provider_cooldown_until: None };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(kinds(&fx), vec!["task.needs_attention"]);
        assert_eq!(t.state, TaskState::NeedsAttention);
    }

    #[test]
    fn idle_within_timeout_does_nothing() {
        let mut t = task(TaskState::Idle, 1);
        let mut s = session(SessionState::Idle, 1);
        let ctx = ProbeContext { now: now(), ..Default::default() };
        assert!(on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx).is_empty());
        let mut t = task(TaskState::Running, 1);
        assert!(on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx).is_empty());
    }

    #[test]
    fn rate_limit_cooldown_passing_resumes_a_live_session() {
        let mut t = task(TaskState::Throttled, 1);
        t.not_before = Some(now() - Duration::seconds(1));
        let mut s = session(SessionState::Running, 1);
        let ctx = ProbeContext { now: now(), ..Default::default() };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(t.not_before, None);
        assert_eq!(kinds(&fx), vec!["task.resumed"]);

        let mut t = task(TaskState::Throttled, 1);
        t.not_before = Some(now() + Duration::minutes(5));
        assert!(on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx).is_empty());
        assert_eq!(t.state, TaskState::Throttled);
    }

    #[test]
    fn non_live_sessions_are_ignored_by_probes() {
        let mut t = task(TaskState::Running, 1);
        let mut s = session(SessionState::Crashed, 1);
        let ctx = ProbeContext { now: now(), ..Default::default() };
        assert!(on_probe(&mut t, &mut s, &dead(Some(1)), &cfg().scheduler, &ctx).is_empty());
    }

    #[test]
    fn crash_without_session_uses_task_attempts() {
        let mut t = task(TaskState::Starting, 1);
        let fx = on_crash(&mut t, None, "setup failed: npm ci", None, &cfg().scheduler, now(), None);
        assert_eq!(t.state, TaskState::Crashed);
        assert_eq!(t.not_before, Some(now() + Duration::seconds(30)));
        assert_eq!(kinds(&fx), vec!["session.crashed"]);
        let mut t = task(TaskState::Starting, 3);
        let fx = on_crash(&mut t, None, "setup failed", None, &cfg().scheduler, now(), None);
        assert_eq!(t.state, TaskState::Failed);
        assert_eq!(kinds(&fx), vec!["task.failed", "cleanup(false)", "linear(Blocked)"]);
    }

    #[test]
    fn preview_flattens_and_truncates() {
        assert_eq!(preview("a\n\n b   c", 10), "a b c");
        assert_eq!(preview("abcdefghij", 5), "abcde…");
    }
}
