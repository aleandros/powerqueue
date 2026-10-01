//! `powerqueue hook` — invoked by Claude Code hooks inside a task session.
//!
//! It must be fast and must never fail the hook: it reads the JSON payload
//! from stdin, stores it in `hook_events` for the daemon, and exits 0. For
//! `Stop` events carrying the done/blocked markers it additionally flips the
//! task state immediately so the dashboard reflects completion even if the
//! daemon is between ticks.

use std::io::Read;

use anyhow::Result;

use crate::domain::{HookEvent, TaskId};
use crate::store::Store;

/// Entry point for the subcommand. Returns the process exit code.
pub fn handle(store: &Store, task_id: TaskId, session_id: Option<uuid::Uuid>, event: HookEvent, stdin: &mut dyn Read) -> Result<i32> {
    let _ = (store, task_id, session_id, event, stdin);
    todo!("TODO(agent-budget): read stdin (may be empty), insert_hook_event, log event; Stop+DONE marker => mark task Completed-pending")
}
