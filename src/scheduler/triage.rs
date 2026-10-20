//! Durable report delivery; analysis completion never resolves the source ticket.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{Task, TaskId};

/// Delivery receipt survives daemon restarts and ambiguous network failures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delivery {
    pub completed_at: DateTime<Utc>,
    pub comment_id: String,
    pub comment_posted: bool,
    pub label_set: bool,
    pub next_attempt: DateTime<Utc>,
    pub error: Option<String>,
}

impl Delivery {
    /// Start a new report; the UUID must be persisted before any network request.
    pub fn new(completed_at: DateTime<Utc>) -> Self {
        Self {
            completed_at,
            comment_id: uuid::Uuid::new_v4().to_string(),
            comment_posted: false,
            label_set: false,
            next_attempt: completed_at,
            error: None,
        }
    }

    /// Both independently retried parts have been confirmed by Linear.
    pub fn done(&self) -> bool {
        self.comment_posted && self.label_set
    }
}

/// Receipt key; a new completion timestamp starts a new report after explicit retry.
pub fn delivery_key(id: TaskId) -> String {
    format!("triage.delivery.{id}")
}

/// Format a report with the workflow's visibility prefix first, before any Markdown.
pub fn report_body(task: &Task, base: Option<&str>) -> String {
    let policy = task.execution.as_ref().and_then(|e| e.policy.as_ref());
    let prefix = policy.map(|p| p.comment_prefix.as_str()).unwrap_or_default();
    let label = task.execution.as_ref().and_then(|e| e.outcome).map(|o| o.label()).unwrap_or("unspecified");
    super::relay::tag_own(&format!(
        "{}## Automated triage\n\n**Recommendation:** `{label}` (not implementation approval)\n\n**Code inspected:** `{}`\n\n{}",
        if prefix.is_empty() { String::new() } else { format!("{prefix}\n\n") },
        base.unwrap_or("unknown"),
        task.summary.as_deref().unwrap_or_default()
    ))
}
