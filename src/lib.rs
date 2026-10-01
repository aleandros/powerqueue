//! # powerqueue
//!
//! An autonomous work queue that turns Linear tickets into Claude Code
//! sessions running inside tmux, one git worktree per task.
//!
//! The crate is organised as a set of mostly independent modules that the
//! [`scheduler`] orchestrates:
//!
//! | module | responsibility |
//! |--------|----------------|
//! | [`config`] | user + repo configuration (TOML), paths |
//! | [`secrets`] | API keys in the OS keychain with an encrypted-file fallback |
//! | [`store`] | SQLite persistence: tasks, sessions, usage, events, commands |
//! | [`linear`] | Linear GraphQL client |
//! | [`priority`] | `PRIORITY.md` rules, live reload, optional Jev scoring |
//! | [`budget`] | model-usage pacing, token accounting, cost estimation |
//! | [`worktree`] / [`tmux`] / [`session`] | runtime: worktrees, tmux windows, Claude Code sessions |
//! | [`scheduler`] | the daemon loop |
//! | [`dashboard`] | live terminal UI |
//! | [`doctor`] | diagnostics and tuning advice |
//! | [`cli`] | clap definitions and command handlers |

pub mod budget;
pub mod cli;
pub mod config;
pub mod dashboard;
pub mod doctor;
pub mod domain;
pub mod hook;
pub mod jev;
pub mod linear;
pub mod logging;
pub mod paths;
pub mod priority;
pub mod scheduler;
pub mod secrets;
pub mod session;
pub mod store;
pub mod tmux;
pub mod worktree;

/// Crate version, injected by Cargo.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Name used for keychain service, tmux session, directories and log targets.
pub const APP_NAME: &str = "powerqueue";
