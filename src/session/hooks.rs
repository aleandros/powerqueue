//! Turn raw hook payloads from Claude Code into scheduler-level outcomes.

use serde::{Deserialize, Serialize};

use crate::domain::{BLOCKED_MARKER, DONE_MARKER, HookEvent};

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

/// `StopFailure.error_type` values that mean "back off, the account is
/// throttled" rather than "this turn broke".
const RATE_LIMIT_ERRORS: [&str; 4] = ["rate_limit", "overloaded", "usage_limit", "quota"];

/// Phrases (lower-case) that mark a message as a request for human input
/// even without the explicit blocked marker. See [`looks_like_question`].
const QUESTION_PHRASES: [&str; 8] = [
    "should i",
    "do you want me to",
    "would you like me to",
    "let me know",
    "which option",
    "which one would you",
    "please confirm",
    "can you clarify",
];

/// Interpret one event. Pure function; see tests in the implementation.
///
/// * `SessionStart` → [`HookOutcome::Started`].
/// * `Stop` → [`HookOutcome::Completed`] when the last assistant message
///   contains [`DONE_MARKER`], [`HookOutcome::Blocked`] when it contains
///   [`BLOCKED_MARKER`] or reads like a question for the human (see
///   [`looks_like_question`]), otherwise [`HookOutcome::TurnEnded`].
/// * `StopFailure` → [`HookOutcome::RateLimited`] for throttling error
///   types, otherwise [`HookOutcome::TurnFailed`].
/// * `SessionEnd` / `Notification` map 1:1; everything else is activity.
pub fn interpret_hook(event: HookEvent, payload: &serde_json::Value) -> HookOutcome {
    let field = |name: &str| payload.get(name).and_then(|v| v.as_str()).map(str::to_string);
    match event {
        HookEvent::SessionStart => HookOutcome::Started {
            transcript_path: field("transcript_path"),
            source: field("source").unwrap_or_else(|| "startup".to_string()),
        },
        HookEvent::Stop => {
            let message = field("last_assistant_message").unwrap_or_default();
            if message.contains(DONE_MARKER) {
                HookOutcome::Completed { summary: text_around_marker(&message, DONE_MARKER) }
            } else if message.contains(BLOCKED_MARKER) {
                HookOutcome::Blocked { reason: text_around_marker(&message, BLOCKED_MARKER) }
            } else if looks_like_question(&message) {
                HookOutcome::Blocked { reason: last_line(&message).to_string() }
            } else {
                HookOutcome::TurnEnded { last_message: message }
            }
        }
        HookEvent::StopFailure => {
            let error_type = field("error_type").unwrap_or_else(|| "unknown".to_string());
            let message = field("error_message").unwrap_or_default();
            if RATE_LIMIT_ERRORS.iter().any(|k| error_type.eq_ignore_ascii_case(k)) {
                HookOutcome::RateLimited { error_type, message }
            } else {
                HookOutcome::TurnFailed { error_type, message }
            }
        }
        HookEvent::SessionEnd => HookOutcome::SessionEnded { reason: field("reason").unwrap_or_else(|| "other".to_string()) },
        HookEvent::Notification => HookOutcome::Notification {
            kind: field("notification_type").unwrap_or_else(|| "unknown".to_string()),
            message: field("message").unwrap_or_default(),
        },
        HookEvent::PreCompact | HookEvent::UserPromptSubmit => HookOutcome::Activity { event },
    }
}

/// Heuristic for "Claude stopped because it is waiting on a human": the last
/// non-empty line ends with a question mark, or the message contains one of
/// `QUESTION_PHRASES` (`should I`, `do you want me to`, `let me know`, ...).
/// False positives only cost a `needs_attention` flag, so the bias is towards
/// catching questions.
pub fn looks_like_question(message: &str) -> bool {
    let last = last_line(message);
    if last.trim_end_matches(['*', '_', '`', ')', '"', '\'']).ends_with('?') {
        return true;
    }
    let lower = message.to_ascii_lowercase();
    QUESTION_PHRASES.iter().any(|p| lower.contains(p))
}

/// Last non-empty line of a message, trimmed (empty string if none).
fn last_line(message: &str) -> &str {
    message.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("")
}

/// Text after `marker`, or the last paragraph before it when nothing follows.
fn text_around_marker(message: &str, marker: &str) -> String {
    let Some(idx) = message.find(marker) else { return message.trim().to_string() };
    let after = message[idx + marker.len()..].trim();
    if !after.is_empty() {
        return after.to_string();
    }
    let before = &message[..idx];
    before
        .rsplit("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .map(|p| p.trim_end_matches(':').trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stop(msg: &str) -> HookOutcome {
        interpret_hook(HookEvent::Stop, &json!({ "last_assistant_message": msg, "stop_hook_active": false }))
    }

    #[test]
    fn session_start_carries_transcript_path() {
        let out = interpret_hook(
            HookEvent::SessionStart,
            &json!({ "session_id": "s", "transcript_path": "/t/s.jsonl", "source": "resume", "model": "claude-fable-5-1" }),
        );
        assert_eq!(out, HookOutcome::Started { transcript_path: Some("/t/s.jsonl".into()), source: "resume".into() });
        let out = interpret_hook(HookEvent::SessionStart, &json!({}));
        assert_eq!(out, HookOutcome::Started { transcript_path: None, source: "startup".into() });
    }

    #[test]
    fn stop_with_done_marker_uses_text_after_it() {
        let out = stop("All tests pass.\n\n[[POWERQUEUE:DONE]] Implemented the parser and added tests.");
        assert_eq!(out, HookOutcome::Completed { summary: "Implemented the parser and added tests.".into() });
    }

    #[test]
    fn stop_with_done_marker_falls_back_to_last_paragraph() {
        let out = stop("I did a lot of work.\n\nSummary: added the thing and fixed the bug.\n\n[[POWERQUEUE:DONE]]\n");
        assert_eq!(out, HookOutcome::Completed { summary: "Summary: added the thing and fixed the bug.".into() });
        assert_eq!(stop("[[POWERQUEUE:DONE]]"), HookOutcome::Completed { summary: String::new() });
    }

    #[test]
    fn done_marker_wins_over_blocked_marker_and_questions() {
        let out = stop("Should I continue?\n[[POWERQUEUE:DONE]] finished");
        assert_eq!(out, HookOutcome::Completed { summary: "finished".into() });
    }

    #[test]
    fn stop_with_blocked_marker() {
        let out = stop("I cannot proceed without the staging credentials.\n\n[[POWERQUEUE:BLOCKED]]");
        assert_eq!(out, HookOutcome::Blocked { reason: "I cannot proceed without the staging credentials.".into() });
        let out = stop("[[POWERQUEUE:BLOCKED]] need the API key");
        assert_eq!(out, HookOutcome::Blocked { reason: "need the API key".into() });
    }

    #[test]
    fn stop_with_question_is_blocked() {
        let out = stop("I found two ways to fix this.\n\nWhich approach do you prefer?");
        assert_eq!(out, HookOutcome::Blocked { reason: "Which approach do you prefer?".into() });
        let out = stop("Should I also update the docs? I'll wait.");
        assert_eq!(out, HookOutcome::Blocked { reason: "Should I also update the docs? I'll wait.".into() });
        let out = stop("Done with part one. Let me know if you want more.");
        assert!(matches!(out, HookOutcome::Blocked { .. }));
        let out = stop("Do you want me to proceed with the migration?**");
        assert!(matches!(out, HookOutcome::Blocked { .. }));
    }

    #[test]
    fn plain_stop_is_turn_ended() {
        let out = stop("I have refactored the module. Running the tests next.");
        assert_eq!(out, HookOutcome::TurnEnded { last_message: "I have refactored the module. Running the tests next.".into() });
        // A question mark in the middle of the message does not count.
        let out = stop("Why does this fail? Because of X. Fixed it.");
        assert!(matches!(out, HookOutcome::TurnEnded { .. }));
        assert_eq!(interpret_hook(HookEvent::Stop, &json!({})), HookOutcome::TurnEnded { last_message: String::new() });
    }

    #[test]
    fn stop_failure_maps_error_types() {
        for et in ["rate_limit", "overloaded", "usage_limit", "quota", "Rate_Limit"] {
            let out = interpret_hook(HookEvent::StopFailure, &json!({ "error_type": et, "error_message": "slow down" }));
            assert_eq!(out, HookOutcome::RateLimited { error_type: et.into(), message: "slow down".into() }, "{et}");
        }
        for et in ["authentication_failed", "server_error", "unknown_thing"] {
            let out = interpret_hook(HookEvent::StopFailure, &json!({ "error_type": et, "error_message": "boom" }));
            assert_eq!(out, HookOutcome::TurnFailed { error_type: et.into(), message: "boom".into() }, "{et}");
        }
        let out = interpret_hook(HookEvent::StopFailure, &json!({}));
        assert_eq!(out, HookOutcome::TurnFailed { error_type: "unknown".into(), message: String::new() });
    }

    #[test]
    fn session_end_and_notification() {
        let out = interpret_hook(HookEvent::SessionEnd, &json!({ "reason": "prompt_input_exit" }));
        assert_eq!(out, HookOutcome::SessionEnded { reason: "prompt_input_exit".into() });
        assert_eq!(interpret_hook(HookEvent::SessionEnd, &json!({})), HookOutcome::SessionEnded { reason: "other".into() });

        let out = interpret_hook(
            HookEvent::Notification,
            &json!({ "notification_type": "permission_prompt", "message": "Claude needs your permission to use Bash" }),
        );
        assert_eq!(
            out,
            HookOutcome::Notification {
                kind: "permission_prompt".into(),
                message: "Claude needs your permission to use Bash".into()
            }
        );
    }

    #[test]
    fn other_events_are_activity() {
        assert_eq!(
            interpret_hook(HookEvent::PreCompact, &json!({ "trigger": "auto" })),
            HookOutcome::Activity { event: HookEvent::PreCompact }
        );
        assert_eq!(
            interpret_hook(HookEvent::UserPromptSubmit, &json!({ "prompt": "hi" })),
            HookOutcome::Activity { event: HookEvent::UserPromptSubmit }
        );
    }

    #[test]
    fn question_heuristic_edge_cases() {
        assert!(looks_like_question("Ready?"));
        assert!(looks_like_question("Is this right?  \n\n"));
        assert!(!looks_like_question(""));
        assert!(!looks_like_question("Done."));
        assert!(looks_like_question("Please confirm the target branch."));
    }
}
