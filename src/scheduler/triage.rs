//! Durable report delivery; analysis completion never resolves the source ticket.

use chrono::{DateTime, Duration, Utc};
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
    /// Failed attempts so far; drives the retry backoff. Older receipts lack it.
    #[serde(default)]
    pub attempts: u32,
}

/// First retry delay after a failed delivery attempt.
const RETRY_BASE: Duration = Duration::seconds(60);
/// Upper bound on the retry delay, so a fixed label setup is picked up within an hour.
const RETRY_CAP: Duration = Duration::hours(1);

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
            attempts: 0,
        }
    }

    /// Record the outcome of an attempt at `now`: a failure doubles the retry delay
    /// (60 s, 120 s, ... capped at one hour); a success clears the error.
    pub fn record_attempt(&mut self, now: DateTime<Utc>, error: Option<String>) {
        if error.is_some() {
            self.attempts = self.attempts.saturating_add(1);
        }
        self.next_attempt = now + retry_delay(self.attempts);
        self.error = error;
    }

    /// Order among due receipts: never-failed reports first, then the longest-waiting
    /// retry, so failing receipts cannot starve newly completed reports.
    pub fn due_order(&self) -> (bool, DateTime<Utc>) {
        (self.attempts > 0, self.next_attempt)
    }

    /// Both independently retried parts have been confirmed by Linear.
    pub fn done(&self) -> bool {
        self.comment_posted && self.label_set
    }
}

/// Delay before the next attempt after `failures` consecutive failed attempts.
pub fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(6);
    (RETRY_BASE * 2i32.pow(doublings)).min(RETRY_CAP)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_back_off_exponentially_to_an_hour_and_success_resets_the_error() {
        let now = Utc::now();
        let mut receipt = Delivery::new(now);
        let delays: Vec<_> = (0..9)
            .map(|_| {
                receipt.record_attempt(now, Some("Label not on issue".into()));
                (receipt.next_attempt - now).num_seconds()
            })
            .collect();
        assert_eq!(delays, [60, 120, 240, 480, 960, 1920, 3600, 3600, 3600]);
        receipt.record_attempt(now, None);
        assert!(receipt.error.is_none());
    }

    #[test]
    fn fresh_receipts_are_tried_before_overdue_retries() {
        let now = Utc::now();
        let mut failing = Delivery::new(now - Duration::days(2));
        failing.record_attempt(now - Duration::days(1), Some("boom".into()));
        let fresh = Delivery::new(now);
        assert!(fresh.due_order() < failing.due_order());
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn retry_delay_is_monotone_and_bounded(a in 0u32..1000, b in 0u32..1000) {
                let (lo, hi) = (a.min(b), a.max(b));
                prop_assert!(retry_delay(lo) <= retry_delay(hi));
                prop_assert!(retry_delay(hi) >= RETRY_BASE && retry_delay(hi) <= RETRY_CAP);
            }

            #[test]
            fn a_never_failed_receipt_sorts_before_any_failed_one(
                fresh_offset in -100_000i64..100_000,
                failed_offset in -100_000i64..100_000,
                failures in 1u32..50,
            ) {
                let now = Utc::now();
                let fresh = Delivery::new(now + Duration::seconds(fresh_offset));
                let mut failed = Delivery::new(now);
                for _ in 0..failures {
                    failed.record_attempt(now + Duration::seconds(failed_offset), Some("e".into()));
                }
                prop_assert!(fresh.due_order() < failed.due_order());
            }
        }
    }
}
