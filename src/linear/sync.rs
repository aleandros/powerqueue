//! Merge fetched Linear issues into the task store.

use anyhow::Result;

use crate::config::LinearConfig;
use crate::domain::TaskId;
use crate::store::Store;

use super::LinearIssue;

/// What a sync pass changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub created: Vec<TaskId>,
    /// Title/description/labels/priority changed on an open task.
    pub updated: Vec<TaskId>,
    /// Open tasks whose issue is no longer queued/in-progress in Linear (closed elsewhere).
    pub cancelled: Vec<TaskId>,
    pub unchanged: usize,
}

/// Reconcile `issues` (the current queued set from Linear) with stored tasks:
///
/// * unknown issue → new `Queued` task (key = identifier);
/// * known open task whose issue changed → update fields (not state);
/// * known open task (queued/throttled/paused/crashed) whose issue is absent
///   from `issues` and, per `fetch_state`, is completed/cancelled in Linear →
///   `Cancelled`. Running tasks are never cancelled by sync; the daemon
///   decides what to do with them.
///
/// Tasks that are already terminal are left alone.
pub fn sync_issues(store: &Store, cfg: &LinearConfig, issues: &[LinearIssue], fetch_state: impl Fn(&str) -> Option<String>) -> Result<SyncReport> {
    let _ = (store, cfg, issues, &fetch_state);
    todo!("TODO(agent-linear)")
}
