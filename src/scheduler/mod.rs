//! The daemon loop.
//!
//! Every tick:
//! 1. drain CLI commands (pause/cancel/retry/...);
//! 2. reload `PRIORITY.md` if it changed; re-score open tasks;
//! 3. poll Linear on its own interval and sync issues into tasks;
//! 4. drain hook events and transcript usage for live sessions, update states;
//! 5. probe tmux panes: dead pane + not completed ⇒ crashed (backoff, resume);
//! 6. detect stale/idle sessions (nudge once, then `needs_attention`);
//! 7. finish completed tasks: cleanup worktree, update Linear; release the
//!    sessions of tasks handed off `in_review`;
//! 8. relay agent questions and human replies through Linear comments
//!    ([`relay`]);
//! 9. watch the pull requests of `in_review` tasks ([`review`]): merged ⇒
//!    completed, conflict / failed check / new review ⇒ relaunch;
//! 10. while slots are free: pick the best schedulable task, ask the budget
//!     policy for a model, launch it.
//!
//! State transitions are logged as events so `task show` reconstructs the story.
//! The decisions themselves live in [`transitions`] as pure functions; the
//! [`daemon`] applies the [`transitions::Effect`]s they return.

pub mod daemon;
pub mod lifecycle;
pub mod relay;
pub mod review;
pub mod transitions;

pub use daemon::{Daemon, DaemonHandle, SKIP_REASON};
pub use lifecycle::{CleanupPlan, cleanup_plan, cleanup_task, pick_next, worktree_dir};
pub use transitions::{CRASH_TAIL_LINES, Effect, LinearTarget, NUDGE_TEXT, ProbeContext, on_crash, on_hook_outcome, on_probe};
