//! Turn raw hook payloads from Claude Code into scheduler-level outcomes.

use serde::{Deserialize, Serialize};

use crate::domain::HookEvent;

/// What a hook event means for the task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HookOutcome {
    /// `SessionStart`: Claude is up; `transcript_path` lets us tail usage.
    Started { transcript_path: Option<String>, source: String },
    /// `Stop` with the done marker in the last message.
    Completed { summary: String },
    /// `Stop` with the blocked marker (or a question) in the last message.
    Blocked { reason: String },
    /// `Stop` without a marker: Claude is waiting for input.
    TurnEnded { last_message: String },
    /// `StopFailure` with `rate_limit` / `overloaded`.
    RateLimited { error_type: String, message: String },
    /// Other `StopFailure`.
    TurnFailed { error_type: String, message: String },
    /// `SessionEnd`.
    SessionEnded { reason: String },
    /// `Notification` (permission prompt, idle prompt, ...).
    Notification { kind: String, message: String },
    /// `PreCompact` / `UserPromptSubmit` and anything else: just activity.
    Activity { event: HookEvent },
}

/// Interpret one event. Pure function; see tests in the implementation.
pub fn interpret_hook(event: HookEvent, payload: &serde_json::Value) -> HookOutcome {
    let _ = (event, payload);
    todo!("TODO(agent-runtime): markers DONE/BLOCKED, question heuristics, StopFailure error_type mapping")
}
