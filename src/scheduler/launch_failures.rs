//! Pure launch-failure circuit policy. Backoff survives passes and daemon restarts.
use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::transitions::Effect;
use crate::domain::{EventLevel, Task};

/// Persistent scheduler state; separate from task retry times and provider budgets.
pub const KEY: &str = "scheduler.launch_failures";

/// Observed failure domain, supplied by the I/O boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureScope {
    /// A setup command may be task-specific. Two consecutive failures in a
    /// project without a successful launch are treated as a shared failure.
    Task,
    /// Checkout, repository configuration or worktree preparation failed.
    Project,
    /// Shared runtime, storage or persistence failed.
    Global,
}

/// Evidence retained until a successful launch demonstrates recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureHold {
    pub failures: u32,
    pub retry_at: Option<DateTime<Utc>>,
    pub reason: String,
}

/// Launch admission holds. Expiry permits a probe; it does not reset backoff.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchFailures {
    pub global: Option<FailureHold>,
    pub projects: BTreeMap<String, FailureHold>,
}

impl LaunchFailures {
    /// Whether a candidate may be tried now. No task, slot or budget is changed.
    pub fn permits(&self, project: &str, now: DateTime<Utc>) -> bool {
        [self.global.as_ref(), self.projects.get(project)].into_iter().flatten().all(|h| h.retry_at.is_none_or(|at| now >= at))
    }

    /// Record a failed start and return its diagnostic event. The first isolated
    /// setup failure allows further work; shared failures wait 30s, doubling to
    /// 15 minutes. Each failed probe extends the hold, even after a restart.
    pub fn failed(&mut self, task: &Task, scope: FailureScope, reason: String, now: DateTime<Utc>) -> Vec<Effect> {
        let hold = if scope == FailureScope::Global {
            self.global.get_or_insert_with(|| FailureHold { failures: 0, retry_at: None, reason: String::new() })
        } else {
            self.projects.entry(task.execution_project.clone()).or_insert_with(|| FailureHold {
                failures: 0,
                retry_at: None,
                reason: String::new(),
            })
        };
        hold.failures = hold.failures.saturating_add(1);
        hold.reason = reason;
        let deferred = scope != FailureScope::Task || hold.failures >= 2;
        if deferred {
            let seconds = (30i64 * (1i64 << hold.failures.saturating_sub(1).min(5))).min(900);
            hold.retry_at = Some(now + Duration::seconds(seconds));
        }
        let effective_scope = if scope == FailureScope::Global {
            "global"
        } else if deferred {
            "project"
        } else {
            "task"
        };
        vec![Effect::log(
            EventLevel::Warn,
            "launch.failure",
            format!(
                "{effective_scope} launch failure: {}; {}",
                hold.reason,
                hold.retry_at.map_or_else(|| "continuing the pass".into(), |at| format!("launches deferred until {at}"))
            ),
            serde_json::json!({ "scope": effective_scope, "project": task.execution_project,
                "failures": hold.failures, "retry_at": hold.retry_at, "reason": hold.reason }),
        )]
    }

    /// A confirmed launch clears this project's streak and any global hold.
    /// Unrelated project holds survive. Emits a recovery event only on change.
    pub fn succeeded(&mut self, task: &Task) -> Vec<Effect> {
        let global = self.global.take().is_some();
        let project = self.projects.remove(&task.execution_project).is_some();
        if !global && !project {
            return Vec::new();
        }
        vec![Effect::log(
            EventLevel::Info,
            "launch.recovered",
            "successful launch cleared failure backoff",
            serde_json::json!({ "project": task.execution_project, "global": global }),
        )]
    }
}

#[cfg(test)]
mod properties {
    use super::*;
    use crate::domain::TaskSource;
    use crate::strategies::{launch_failure_scope, origin};
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn outages_bound_attempts_survive_restart_and_recover(
            scope in launch_failure_scope(), task_count in 3usize..40, ticks in 1i64..120,
        ) {
            let mut failures = LaunchFailures::default();
            let task = Task::new("FAIL", "broken", TaskSource::Manual);
            let mut attempts = 0;
            let mut last_batch = None;
            for tick in 0..ticks {
                let now = origin() + Duration::seconds(tick);
                let mut batch = 0;
                for _ in 0..task_count {
                    if failures.permits("default", now) {
                        failures.failed(&task, scope, "outage".into(), now);
                        attempts += 1;
                        batch += 1;
                    }
                }
                if batch > 0 {
                    prop_assert!(batch <= if scope == FailureScope::Task && last_batch.is_none() { 2 } else { 1 }, "unbounded failure batch");
                    if let Some(previous) = last_batch { prop_assert!(tick - previous >= 30); }
                    last_batch = Some(tick);
                }
                failures = serde_json::from_str(&serde_json::to_string(&failures).unwrap()).unwrap();
                prop_assert_eq!(failures.permits("unrelated", now), scope != FailureScope::Global);
            }
            prop_assert!(attempts <= 2 + ticks / 30);
            let recovered = origin() + Duration::seconds(ticks + 900);
            prop_assert!(failures.permits("default", recovered), "a repaired project must eventually get a probe");
            prop_assert!(!failures.succeeded(&task).is_empty());
            prop_assert_eq!(failures, LaunchFailures::default());
        }
    }

    #[test]
    fn isolated_setup_failure_continues_and_success_resets_only_its_project() {
        let mut failures = LaunchFailures::default();
        let mut task = Task::new("A", "setup failed", TaskSource::Manual);
        failures.failed(&task, FailureScope::Task, "task-specific".into(), origin());
        assert!(failures.permits("default", origin()));
        task.execution_project = "other".into();
        failures.failed(&task, FailureScope::Project, "missing checkout".into(), origin());
        task.execution_project = "default".into();
        failures.succeeded(&task);
        assert!(!failures.projects.contains_key("default"));
        assert!(!failures.permits("other", origin()));
    }
}
