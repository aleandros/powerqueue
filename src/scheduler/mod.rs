//! The daemon loop.
//!
//! Every tick:
//! 1. drain CLI commands (pause/cancel/retry/...);
//! 2. reload `PRIORITY.md` if it changed; re-score open tasks;
//! 3. poll Linear on its own interval and sync issues into tasks;
//! 4. drain hook events and transcript usage for live sessions, update states;
//! 5. probe tmux panes: dead pane + not completed ⇒ crashed (backoff, resume);
//! 6. detect stale/idle sessions (nudge once, then `needs_attention`);
//! 7. finish completed tasks: cleanup worktree, update Linear;
//! 8. while slots are free: pick the best schedulable task, ask the budget
//!    policy for a model, launch it.
//!
//! State transitions are logged as events so `task show` reconstructs the story.

pub mod daemon;
pub mod lifecycle;

pub use daemon::{Daemon, DaemonHandle};
pub use lifecycle::{cleanup_task, pick_next};
