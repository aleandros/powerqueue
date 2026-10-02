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
use crate::domain::{EventLevel, HookEvent, ModelTier, Session, SessionState, Task, TaskState};
use crate::session::{HookOutcome, SessionProbe};

/// What Claude is told when a session sits idle without a completion signal.
pub const NUDGE_TEXT: &str = "If the task is complete, run the completion command and print the done marker; otherwise continue.";

/// How many pane lines the daemon captures for crash reports.
pub const CRASH_TAIL_LINES: u32 = 40;

/// Which Linear workflow transition an effect asks for; the daemon maps it
/// to the configured state name (if any).
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
    /// Move the Linear issue and/or post a comment (best effort).
    Linear { target: LinearTarget, comment: Option<String> },
    /// Type text into the session's pane.
    Nudge { text: String },
    /// Kill the session's tmux window.
    KillWindow,
    /// Record a rate-limit cooldown for a tier.
    RateLimit { tier: ModelTier, until: DateTime<Utc> },
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
}

/// Apply a hook outcome. `period_end` caps rate-limit cooldowns (the
/// allowance resets there anyway).
pub fn on_hook_outcome(
    task: &mut Task,
    session: &mut Session,
    outcome: &HookOutcome,
    cfg: &Config,
    now: DateTime<Utc>,
    period_end: DateTime<Utc>,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    session.last_activity_at = now;
    match outcome {
        HookOutcome::Started { transcript_path, source } => {
            if transcript_path.is_some() {
                session.transcript_path = transcript_path.clone();
            }
            if session.state.is_live() {
                session.state = SessionState::Running;
            }
            if matches!(task.state, TaskState::Starting | TaskState::Idle | TaskState::NeedsAttention | TaskState::Crashed) {
                task.state = TaskState::Running;
            }
            effects.push(Effect::log(
                EventLevel::Info,
                "session.started",
                format!("Claude Code session started ({source})"),
                serde_json::json!({ "source": source, "transcript_path": transcript_path, "attempt": session.attempt }),
            ));
        }
        HookOutcome::Completed { summary } => {
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
        HookOutcome::Blocked { reason } => {
            task.state = TaskState::NeedsAttention;
            task.last_error = Some(reason.clone());
            session.state = SessionState::Idle;
            effects.push(Effect::log(
                EventLevel::Warn,
                "task.blocked",
                format!("Claude reports a blocker: {reason}"),
                serde_json::json!({ "reason": reason }),
            ));
            effects.push(Effect::Linear {
                target: LinearTarget::Blocked,
                comment: Some(format!(
                    "powerqueue needs a human on this task (branch `{}`):\n\n{reason}",
                    task.branch.as_deref().unwrap_or("?")
                )),
            });
        }
        HookOutcome::TurnEnded { last_message } => {
            session.state = SessionState::Idle;
            if matches!(task.state, TaskState::Running | TaskState::Starting) {
                task.state = TaskState::Idle;
            }
            effects.push(Effect::log(
                EventLevel::Debug,
                "session.turn_ended",
                "turn ended without a completion marker",
                serde_json::json!({ "preview": preview(last_message, 200) }),
            ));
        }
        HookOutcome::RateLimited { error_type, message } => {
            let cooldown = Duration::minutes(cfg.budget.rate_limit_cooldown_mins.max(1) as i64);
            let until = (now + cooldown).min(period_end).max(now + Duration::minutes(1));
            task.state = TaskState::Throttled;
            task.not_before = Some(until);
            task.last_error = Some(format!("{error_type}: {message}"));
            // Subscription limits (5-hour window, weekly cap) are account-wide,
            // so a `rate_limit` pauses every tier; `overloaded` is specific to
            // the model that reported it.
            let account_wide = error_type != "overloaded";
            if account_wide {
                for tier in ModelTier::ALL {
                    effects.push(Effect::RateLimit { tier, until });
                }
            } else {
                effects.push(Effect::RateLimit { tier: session.model, until });
            }
            effects.push(Effect::log(
                EventLevel::Warn,
                "budget.rate_limited",
                format!(
                    "{} reported {error_type}; cooling down until {}",
                    session.model,
                    until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                ),
                serde_json::json!({ "tier": session.model, "error_type": error_type, "message": message, "until": until }),
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
            if task.state.is_terminal() || task.state == TaskState::Paused {
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
                task.last_error = Some(format!("waiting for permission: {message}"));
                effects.push(Effect::log(
                    EventLevel::Warn,
                    "session.permission_prompt",
                    format!("Claude is waiting for a permission decision: {message}"),
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
                    "Claude is waiting for input",
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
            if *event == HookEvent::UserPromptSubmit && session.state == SessionState::Idle {
                session.state = SessionState::Running;
                if task.state == TaskState::Idle {
                    task.state = TaskState::Running;
                }
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
        if task.state.is_terminal() || task.state == TaskState::Paused {
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
            (true, Some(code)) => format!("Claude exited with status {code}"),
            (true, None) => "Claude exited".to_string(),
        };
        return on_crash(task, Some(session), &reason, probe.exit_status, cfg, now, ctx.pane_tail.as_deref());
    }

    // Alive.
    if task.state == TaskState::Throttled && task.not_before.is_some_and(|t| t <= now) {
        task.state = TaskState::Running;
        task.not_before = None;
        session.state = SessionState::Running;
        effects.push(Effect::log(
            EventLevel::Info,
            "task.resumed",
            "rate-limit cooldown passed; session still alive",
            serde_json::json!({}),
        ));
        return effects;
    }

    let since_activity = now - session.last_activity_at;
    let running = matches!(task.state, TaskState::Running | TaskState::Starting);
    if running && cfg.stale_session_secs > 0 && since_activity > Duration::seconds(cfg.stale_session_secs as i64) {
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
    let age = now - session.started_at;
    if cfg.max_session_secs > 0 && !task.state.is_terminal() && age > Duration::seconds(cfg.max_session_secs as i64) {
        let reason = format!("attempt ran for {}s (max_session_secs = {})", age.num_seconds(), cfg.max_session_secs);
        effects.push(Effect::log(
            EventLevel::Warn,
            "session.timeout",
            reason.clone(),
            serde_json::json!({ "age_secs": age.num_seconds() }),
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
    let data = serde_json::json!({ "reason": reason, "exit_status": exit_status, "attempt": attempt, "max_attempts": max, "pane_tail": pane_tail });
    if attempt >= max {
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
        let backoff = Duration::seconds(cfg.backoff_for_attempt(attempt) as i64);
        task.state = TaskState::Crashed;
        task.not_before = Some(now + backoff);
        effects.push(Effect::log(
            EventLevel::Warn,
            "session.crashed",
            format!("{reason}; retrying in {}s (attempt {attempt} of {max})", backoff.num_seconds()),
            data,
        ));
    }
    effects
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
            model: ModelTier::Opus,
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
            })
            .collect()
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
        let fx =
            on_hook_outcome(&mut t, &mut s, &HookOutcome::Blocked { reason: "need creds".into() }, &cfg(), now(), period_end());
        assert_eq!(t.state, TaskState::NeedsAttention);
        assert_eq!(t.last_error.as_deref(), Some("need creds"));
        assert_eq!(s.state, SessionState::Idle);
        assert_eq!(kinds(&fx), vec!["task.blocked", "linear(Blocked)"]);
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
        for tier in ModelTier::ALL {
            assert!(fx.contains(&Effect::RateLimit { tier, until: now() + Duration::minutes(30) }), "rate_limit is account-wide");
        }
        assert_eq!(kinds(&fx), vec!["ratelimit", "ratelimit", "ratelimit", "ratelimit", "budget.rate_limited"]);

        let mut t = task(TaskState::Running, 1);
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), now() + Duration::minutes(5));
        assert_eq!(t.not_before, Some(now() + Duration::minutes(5)), "capped at the period end");
        assert!(matches!(fx[0], Effect::RateLimit { until, .. } if until == now() + Duration::minutes(5)));

        let mut t = task(TaskState::Running, 1);
        let out = HookOutcome::RateLimited { error_type: "overloaded".into(), message: "busy".into() };
        let fx = on_hook_outcome(&mut t, &mut s, &out, &cfg(), now(), period_end());
        assert_eq!(fx[0], Effect::RateLimit { tier: ModelTier::Opus, until: now() + Duration::minutes(30) });
        assert_eq!(kinds(&fx), vec!["ratelimit", "budget.rate_limited"], "overloaded only affects the reporting tier");
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
        let ctx = ProbeContext { now: now(), nudged: false, pane_tail: Some("boom".into()) };
        let fx = on_probe(&mut t, &mut s, &dead(Some(1)), &cfg().scheduler, &ctx);
        assert_eq!(t.state, TaskState::Crashed);
        assert_eq!(t.not_before, Some(now() + Duration::seconds(120)), "second attempt uses the second backoff");
        assert_eq!(t.last_error.as_deref(), Some("Claude exited with status 1"));
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
    fn dead_pane_for_paused_or_finished_task_just_exits() {
        for state in [TaskState::Paused, TaskState::Completed, TaskState::Cancelled, TaskState::Failed] {
            let mut t = task(state, 1);
            let mut s = session(SessionState::Running, 1);
            let ctx = ProbeContext { now: now(), ..Default::default() };
            let fx = on_probe(&mut t, &mut s, &dead(Some(0)), &cfg().scheduler, &ctx);
            assert_eq!(t.state, state);
            assert_eq!(s.state, SessionState::Exited);
            assert_eq!(kinds(&fx), vec!["session.exited"]);
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
    fn idle_session_is_nudged_once_then_needs_attention() {
        let mut t = task(TaskState::Idle, 1);
        let mut s = session(SessionState::Idle, 1);
        s.last_activity_at = now() - Duration::minutes(11);
        let ctx = ProbeContext { now: now(), nudged: false, pane_tail: None };
        let fx = on_probe(&mut t, &mut s, &alive(), &cfg().scheduler, &ctx);
        assert_eq!(kinds(&fx), vec!["nudge", "session.nudged"]);
        assert_eq!(fx[0], Effect::Nudge { text: NUDGE_TEXT.to_string() });
        assert_eq!(s.last_activity_at, now(), "the second timeout counts from the nudge");
        assert_eq!(t.state, TaskState::Idle);

        s.last_activity_at = now() - Duration::minutes(11);
        let ctx = ProbeContext { now: now(), nudged: true, pane_tail: None };
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
