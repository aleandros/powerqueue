//! Task selection and teardown helpers (pure where possible, for tests).

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::config::Config;
use crate::domain::{Task, TaskState};
use crate::store::Store;
use crate::tmux::Tmux;
use crate::worktree::Repo;

/// Choose the next task to start: highest score among schedulable tasks whose
/// `not_before` has passed; ties broken by criticality then age.
pub fn pick_next(tasks: &[Task], now: DateTime<Utc>) -> Option<&Task> {
    let _ = (tasks, now, TaskState::Queued);
    todo!("TODO(agent-budget)")
}

/// Release a finished task's resources according to config (push, remove
/// worktree, delete branch, close tmux window, run cleanup commands).
/// Never deletes unpushed work: if `push_branch` fails or there is no
/// remote, the worktree is kept and an event explains why.
pub fn cleanup_task(cfg: &Config, store: &Store, repo: &Repo, tmux: &Tmux, task: &mut Task, succeeded: bool) -> Result<()> {
    let _ = (cfg, store, repo, tmux, task, succeeded);
    todo!("TODO(agent-budget)")
}
