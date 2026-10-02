//! `powerqueue hook` — invoked by Claude Code hooks inside a task session.
//!
//! It must be fast and must never fail the hook: it reads the JSON payload
//! from stdin, stores it in `hook_events` for the daemon, and exits 0. For
//! `Stop` events carrying the done/blocked markers it additionally flips the
//! task state immediately so the dashboard reflects completion even if the
//! daemon is between ticks.

use std::io::Read;

use anyhow::Result;
use chrono::Utc;

use crate::domain::{BLOCKED_MARKER, DONE_MARKER, EventLevel, HookEvent, TaskId, TaskState};
use crate::store::Store;

/// Longest payload preview stored in the `hook.*` event.
const PREVIEW_CHARS: usize = 200;

/// Entry point for the subcommand. Returns the process exit code.
///
/// Always returns `Ok(0)`: a hook that fails would block Claude Code, so
/// store errors are only traced. Reads stdin fully; an empty or non-JSON
/// payload is stored as `{}`.
pub fn handle(
    store: &Store,
    task_id: TaskId,
    session_id: Option<uuid::Uuid>,
    event: HookEvent,
    stdin: &mut dyn Read,
) -> Result<i32> {
    let mut raw = String::new();
    if let Err(e) = stdin.read_to_string(&mut raw) {
        tracing::warn!(error = %e, "hook: cannot read stdin; storing an empty payload");
    }
    let payload: serde_json::Value = match serde_json::from_str(raw.trim()) {
        Ok(v @ serde_json::Value::Object(_)) => v,
        Ok(other) => serde_json::json!({ "value": other }),
        Err(_) => serde_json::json!({}),
    };
    let session_id =
        session_id.or_else(|| payload.get("session_id").and_then(|s| s.as_str()).and_then(|s| uuid::Uuid::parse_str(s).ok()));

    if let Err(e) = store.insert_hook_event(task_id, session_id, event, &payload) {
        tracing::error!(task = %task_id, event = event.as_str(), error = %e, "hook: cannot store event");
        return Ok(0);
    }
    let kind = format!("hook.{}", event.as_str().to_ascii_lowercase());
    let preview: String = payload.to_string().chars().take(PREVIEW_CHARS).collect();
    if let Err(e) = store.log_event(
        Some(task_id),
        session_id,
        EventLevel::Debug,
        &kind,
        &preview,
        serde_json::json!({ "event": event.as_str() }),
    ) {
        tracing::warn!(error = %e, "hook: cannot log event");
    }

    if event == HookEvent::Stop
        && let Some(message) = payload.get("last_assistant_message").and_then(|m| m.as_str())
        && let Err(e) = apply_markers(store, task_id, message)
    {
        tracing::warn!(task = %task_id, error = %format!("{e:#}"), "hook: cannot apply completion marker");
    }
    Ok(0)
}

/// Flip the task state when the final message carries a marker. The daemon
/// still processes the stored hook row for cleanup and Linear updates.
fn apply_markers(store: &Store, task_id: TaskId, message: &str) -> Result<()> {
    let Some(mut task) = store.get_task(task_id)? else { return Ok(()) };
    if task.state.is_terminal() {
        return Ok(());
    }
    if let Some(summary) = text_after(message, DONE_MARKER) {
        task.state = TaskState::Completed;
        task.completed_at = Some(Utc::now());
        task.last_error = None;
        if !summary.is_empty() {
            task.summary = Some(summary);
        }
        store.update_task(&task)?;
        store.log_event(
            Some(task_id),
            None,
            EventLevel::Info,
            "task.completed",
            "done marker received",
            serde_json::json!({ "summary": task.summary }),
        )?;
    } else if let Some(reason) = text_after(message, BLOCKED_MARKER) {
        task.state = TaskState::NeedsAttention;
        task.last_error = Some(if reason.is_empty() { "Claude reported a blocker".to_string() } else { reason });
        store.update_task(&task)?;
        store.log_event(
            Some(task_id),
            None,
            EventLevel::Warn,
            "task.blocked",
            "blocked marker received",
            serde_json::json!({ "reason": task.last_error }),
        )?;
    }
    Ok(())
}

/// Text following the first occurrence of `marker`, trimmed (`None` when absent).
fn text_after(message: &str, marker: &str) -> Option<String> {
    message.find(marker).map(|i| message[i + marker.len()..].trim().to_string())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::domain::{Task, TaskSource};

    fn store_with_task() -> (Store, TaskId) {
        let store = Store::open_in_memory().unwrap();
        let mut t = Task::new("ENG-1", "t", TaskSource::Manual);
        t.state = TaskState::Running;
        store.insert_task(&t).unwrap();
        (store, t.id)
    }

    #[test]
    fn stores_payload_and_logs_event() {
        let (store, id) = store_with_task();
        let sid = uuid::Uuid::new_v4();
        let mut stdin = Cursor::new(format!(r#"{{"session_id":"{sid}","transcript_path":"/t.jsonl","source":"startup"}}"#));
        let code = handle(&store, id, None, HookEvent::SessionStart, &mut stdin).unwrap();
        assert_eq!(code, 0);
        let events = store.drain_hook_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, HookEvent::SessionStart);
        assert_eq!(events[0].session_id, Some(sid), "session id is taken from the payload when not passed");
        assert_eq!(events[0].payload["transcript_path"], "/t.jsonl");
        let log = store.events_for_task(id, 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].kind, "hook.sessionstart");
        assert!(log[0].message.contains("transcript_path"));
        assert_eq!(store.get_task(id).unwrap().unwrap().state, TaskState::Running);
    }

    #[test]
    fn empty_or_invalid_stdin_is_stored_as_empty_object() {
        let (store, id) = store_with_task();
        assert_eq!(handle(&store, id, None, HookEvent::Notification, &mut Cursor::new("")).unwrap(), 0);
        assert_eq!(handle(&store, id, None, HookEvent::PreCompact, &mut Cursor::new("not json")).unwrap(), 0);
        assert_eq!(handle(&store, id, None, HookEvent::PreCompact, &mut Cursor::new("[1,2]")).unwrap(), 0);
        let events = store.drain_hook_events().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].payload, serde_json::json!({}));
        assert_eq!(events[1].payload, serde_json::json!({}));
        assert_eq!(events[2].payload, serde_json::json!({ "value": [1, 2] }));
    }

    #[test]
    fn done_marker_completes_the_task_immediately() {
        let (store, id) = store_with_task();
        let payload = serde_json::json!({ "last_assistant_message": format!("All tests pass.\n{DONE_MARKER} Implemented the feature and pushed.") });
        handle(&store, id, None, HookEvent::Stop, &mut Cursor::new(payload.to_string())).unwrap();
        let task = store.get_task(id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!(task.summary.as_deref(), Some("Implemented the feature and pushed."));
        assert!(task.completed_at.is_some());
        let kinds: Vec<String> = store.events_for_task(id, 10).unwrap().into_iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec!["hook.stop", "task.completed"]);
        assert_eq!(store.drain_hook_events().unwrap().len(), 1, "the daemon still sees the row");
    }

    #[test]
    fn blocked_marker_needs_attention() {
        let (store, id) = store_with_task();
        let payload =
            serde_json::json!({ "last_assistant_message": format!("{BLOCKED_MARKER}\nI need the staging credentials.") });
        handle(&store, id, None, HookEvent::Stop, &mut Cursor::new(payload.to_string())).unwrap();
        let task = store.get_task(id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::NeedsAttention);
        assert_eq!(task.last_error.as_deref(), Some("I need the staging credentials."));

        let payload = serde_json::json!({ "last_assistant_message": BLOCKED_MARKER });
        let (store, id) = store_with_task();
        handle(&store, id, None, HookEvent::Stop, &mut Cursor::new(payload.to_string())).unwrap();
        assert_eq!(store.get_task(id).unwrap().unwrap().last_error.as_deref(), Some("Claude reported a blocker"));
    }

    #[test]
    fn stop_without_marker_or_terminal_task_leaves_state_alone() {
        let (store, id) = store_with_task();
        let payload = serde_json::json!({ "last_assistant_message": "still working" });
        handle(&store, id, None, HookEvent::Stop, &mut Cursor::new(payload.to_string())).unwrap();
        assert_eq!(store.get_task(id).unwrap().unwrap().state, TaskState::Running);

        let mut task = store.get_task(id).unwrap().unwrap();
        task.state = TaskState::Cancelled;
        store.update_task(&task).unwrap();
        let payload = serde_json::json!({ "last_assistant_message": format!("{DONE_MARKER} late") });
        handle(&store, id, None, HookEvent::Stop, &mut Cursor::new(payload.to_string())).unwrap();
        assert_eq!(store.get_task(id).unwrap().unwrap().state, TaskState::Cancelled, "terminal tasks are not revived");
    }

    #[test]
    fn unknown_task_still_returns_zero() {
        let store = Store::open_in_memory().unwrap();
        let payload = serde_json::json!({ "last_assistant_message": DONE_MARKER });
        let code = handle(&store, TaskId::new(), None, HookEvent::Stop, &mut Cursor::new(payload.to_string())).unwrap();
        assert_eq!(code, 0);
        assert_eq!(store.drain_hook_events().unwrap().len(), 1);
    }

    #[test]
    fn text_after_marker() {
        assert_eq!(text_after("x [[M]] y ", "[[M]]").as_deref(), Some("y"));
        assert_eq!(text_after("[[M]]", "[[M]]").as_deref(), Some(""));
        assert_eq!(text_after("nothing", "[[M]]"), None);
    }
}
