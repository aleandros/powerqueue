//! `powerqueue doctor` — diagnostics and tuning advice.
//!
//! Each check returns a [`CheckResult`] with a status, a one-line finding and
//! an optional fix hint. `--fix` applies the safe automatic fixes (prune
//! orphan worktrees/windows, reset stuck states, clear stale locks).
//!
//! Categories:
//! * environment: git, tmux, claude binaries + versions, `claude auth status`
//! * configuration: config.toml validity, PRIORITY.md parse, repo path, worktree root writable
//! * secrets: keychain backend, Linear key works (`viewer`), Jev key (if enabled)
//! * state: database integrity, daemon heartbeat, orphaned worktrees / tmux windows,
//!   tasks stuck in `starting`/`running` with no live session
//! * algorithm: budget anchor set, estimator accuracy, Fable under/over-reservation,
//!   crash rate, idle rate, throttling frequency — each with a concrete suggestion

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::paths::Paths;
use crate::secrets::Secrets;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub category: String,
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix_hint: Option<String>,
    /// Set when `--fix` repaired the problem.
    pub fixed: bool,
}

/// Run every check. `fix` applies safe repairs. `online` allows network calls.
pub async fn run_all(
    cfg: &Config,
    paths: &Paths,
    store: &Store,
    secrets: &Secrets,
    fix: bool,
    online: bool,
) -> Result<Vec<CheckResult>> {
    let _ = (cfg, paths, store, secrets, fix, online);
    todo!("TODO(agent-ux)")
}

/// Exit code: 0 all ok/warn, 1 if any failure.
pub fn exit_code(results: &[CheckResult]) -> i32 {
    if results.iter().any(|r| r.status == Status::Fail) { 1 } else { 0 }
}
