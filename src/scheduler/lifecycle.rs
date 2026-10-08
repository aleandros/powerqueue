//! Task selection and teardown helpers (pure where possible, for tests).

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{CleanupConfig, Config};
use crate::domain::{EventLevel, Task};
use crate::store::Store;
use crate::tmux::Tmux;
use crate::worktree::{Repo, run_commands};

/// Choose the next task to start: highest score among schedulable tasks whose
/// `not_before` has passed and that wait on nothing ([`Task::is_waiting`]:
/// no pending blocker, not a container); ties broken by criticality then age.
pub fn pick_next(tasks: &[Task], now: DateTime<Utc>) -> Option<&Task> {
    tasks.iter().filter(|t| t.state.is_schedulable() && !t.is_waiting() && t.not_before.is_none_or(|nb| nb <= now)).min_by(
        |a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.criticality.cmp(&b.criticality))
                .then(a.created_at.cmp(&b.created_at))
        },
    )
}

/// What [`cleanup_task`] will do, decided from config and the repository facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupPlan {
    /// Push the branch to the remote first.
    pub push: bool,
    /// Remove the worktree (only when nothing would be lost).
    pub remove_worktree: bool,
    /// Delete the local branch after removing the worktree.
    pub delete_branch: bool,
    /// Kill the task's tmux window.
    pub close_window: bool,
    /// Why the worktree is kept, when it is.
    pub keep_reason: Option<String>,
}

/// Pure decision for [`cleanup_task`].
///
/// * `succeeded`: the task completed (false = failed/cancelled).
/// * `has_remote`: the repository has a remote to push to.
/// * `unpushed`: commits on the branch that exist nowhere else (0 when pushed).
/// * `pushed_ok`: the push attempt succeeded (ignored when no push was planned).
///
/// The worktree is never removed while it holds unpushed commits.
pub fn cleanup_plan(cfg: &CleanupConfig, succeeded: bool, has_remote: bool, unpushed: u32, pushed_ok: bool) -> CleanupPlan {
    let push = cfg.push_branch && has_remote;
    let (remove_worktree, keep_reason) = if !succeeded && cfg.keep_failed {
        (false, Some("cleanup.keep_failed is set and the task did not succeed".to_string()))
    } else if !cfg.remove_worktree {
        (false, Some("cleanup.remove_worktree is false".to_string()))
    } else if push && !pushed_ok {
        (false, Some("push failed; keeping the worktree so no work is lost".to_string()))
    } else if unpushed > 0 && !(push && pushed_ok) {
        let why = if cfg.push_branch && !has_remote { "no remote to push to" } else { "cleanup.push_branch is false" };
        (false, Some(format!("{unpushed} unpushed commit(s) and {why}")))
    } else {
        (true, None)
    };
    CleanupPlan {
        push,
        remove_worktree,
        delete_branch: remove_worktree && cfg.delete_branch,
        close_window: cfg.close_tmux_window,
        keep_reason,
    }
}

/// Release a finished task's resources according to config (push, remove
/// worktree, delete branch, close tmux window, run cleanup commands).
/// Never deletes unpushed work: if `push_branch` fails or there is no
/// remote, the worktree is kept and an event explains why.
///
/// `for_review` releases a task handed off `in_review`: like a success, but
/// the local branch is always kept (a review round recreates the worktree
/// from it) and the tmux window is always closed (the session is resumed
/// later, never left running).
///
/// Mutates `task` (`worktree_path`, `last_error`) but does not persist it;
/// the caller saves the task. Individual git/tmux failures are logged as
/// `cleanup.*` events and do not abort the remaining steps; only a store
/// failure is returned as an error.
pub fn cleanup_task(
    cfg: &Config,
    store: &Store,
    repo: &Repo,
    tmux: &Tmux,
    task: &mut Task,
    succeeded: bool,
    for_review: bool,
) -> Result<()> {
    let succeeded = succeeded || for_review;
    let log = |level: EventLevel, kind: &str, message: &str, data: serde_json::Value| -> Result<()> {
        store.log_event(Some(task.id), None, level, kind, message, data)?;
        Ok(())
    };
    let branch = task.branch.clone();
    let worktree = task.worktree_path.clone().map(std::path::PathBuf::from);
    let mut pushed_ok = false;
    let mut commands_ok = true;

    if let Some(wt) = worktree.as_deref().filter(|p| p.exists()) {
        if !cfg.cleanup.run.is_empty() {
            let env = cleanup_env(task, succeeded);
            match run_commands(wt, &cfg.cleanup.run, &env) {
                Ok(output) => {
                    log(
                        EventLevel::Info,
                        "cleanup.commands",
                        "cleanup commands finished",
                        serde_json::json!({ "output": tail(&output, 2000) }),
                    )?;
                }
                Err(e) => {
                    commands_ok = false;
                    task.last_error = Some(format!("cleanup command failed: {e:#}"));
                    tracing::warn!(task = %task.key, error = %format!("{e:#}"), "cleanup command failed");
                    log(
                        EventLevel::Warn,
                        "cleanup.command_failed",
                        &format!("cleanup command failed: {e:#}"),
                        serde_json::json!({}),
                    )?;
                }
            }
        }

        if cfg.cleanup.commit_uncommitted {
            match repo.is_dirty(wt) {
                Ok(true) => {
                    let message = format!("powerqueue: uncommitted changes from {}", task.key);
                    match repo.commit_all(wt, &message) {
                        Ok(_) => log(
                            EventLevel::Warn,
                            "cleanup.autocommit",
                            "the session left uncommitted changes; committed them so they are not lost",
                            serde_json::json!({ "message": message }),
                        )?,
                        Err(e) => {
                            commands_ok = false;
                            tracing::warn!(task = %task.key, error = %format!("{e:#}"), "cannot commit leftover changes");
                            log(
                                EventLevel::Error,
                                "cleanup.autocommit_failed",
                                &format!("could not commit leftover changes; keeping the worktree: {e:#}"),
                                serde_json::json!({}),
                            )?;
                        }
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(task = %task.key, error = %format!("{e:#}"), "cannot check whether the worktree is dirty")
                }
            }
        }

        let has_remote = match repo.has_remote() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(task = %task.key, error = %format!("{e:#}"), "cannot determine whether the repo has a remote");
                false
            }
        };
        let push_wanted = cfg.cleanup.push_branch && has_remote;
        if push_wanted && let Some(branch) = branch.as_deref() {
            match repo.push_branch(wt, branch) {
                Ok(()) => {
                    pushed_ok = true;
                    log(
                        EventLevel::Info,
                        "cleanup.pushed",
                        &format!("pushed branch {branch}"),
                        serde_json::json!({ "branch": branch }),
                    )?;
                }
                Err(e) => {
                    task.last_error = Some(format!("push of {branch} failed: {e:#}"));
                    tracing::warn!(task = %task.key, branch, error = %format!("{e:#}"), "push failed; keeping worktree");
                    log(
                        EventLevel::Warn,
                        "cleanup.push_failed",
                        &format!("push of {branch} failed; keeping the worktree: {e:#}"),
                        serde_json::json!({ "branch": branch }),
                    )?;
                }
            }
        }

        let unpushed = if pushed_ok {
            0
        } else {
            match (branch.as_deref(), base_branch(cfg, repo)) {
                (Some(b), Some(base)) => repo.unpushed_commits(wt, b, &base).unwrap_or_else(|e| {
                    tracing::warn!(task = %task.key, error = %format!("{e:#}"), "cannot count unpushed commits; assuming some");
                    1
                }),
                _ => 1,
            }
        };

        let mut plan = cleanup_plan(&cfg.cleanup, succeeded, has_remote, unpushed, pushed_ok);
        if for_review {
            plan.delete_branch = false;
            plan.close_window = true;
        }
        if !commands_ok && plan.remove_worktree {
            plan.remove_worktree = false;
            plan.delete_branch = false;
            plan.keep_reason = Some("a cleanup command failed; keeping the worktree for inspection".to_string());
        }

        let mut removed = false;
        let mut branch_deleted = false;
        if plan.remove_worktree {
            match repo.remove_worktree(wt, true) {
                Ok(()) => {
                    removed = true;
                    task.worktree_path = None;
                    if plan.delete_branch
                        && let Some(branch) = branch.as_deref()
                    {
                        match repo.delete_branch(branch, true) {
                            Ok(()) => branch_deleted = true,
                            Err(e) => {
                                tracing::warn!(task = %task.key, branch, error = %format!("{e:#}"), "branch deletion failed");
                                log(
                                    EventLevel::Warn,
                                    "cleanup.branch_delete_failed",
                                    &format!("could not delete {branch}: {e:#}"),
                                    serde_json::json!({}),
                                )?;
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(task = %task.key, path = %wt.display(), error = %format!("{e:#}"), "worktree removal failed");
                    log(
                        EventLevel::Warn,
                        "cleanup.remove_failed",
                        &format!("could not remove worktree: {e:#}"),
                        serde_json::json!({ "path": wt }),
                    )?;
                }
            }
        } else if let Some(why) = &plan.keep_reason {
            log(EventLevel::Info, "cleanup.kept", &format!("worktree kept: {why}"), serde_json::json!({ "path": wt }))?;
        }

        let window_closed = if plan.close_window { close_window(store, tmux, task) } else { false };
        log(
            EventLevel::Info,
            "cleanup.done",
            &format!(
                "cleanup finished ({})",
                if for_review {
                    "in review; branch kept"
                } else if succeeded {
                    "succeeded"
                } else {
                    "did not succeed"
                }
            ),
            serde_json::json!({
                "succeeded": succeeded,
                "for_review": for_review,
                "pushed": pushed_ok,
                "worktree_removed": removed,
                "branch_deleted": branch_deleted,
                "window_closed": window_closed,
                "keep_reason": plan.keep_reason,
                "unpushed": unpushed,
            }),
        )?;
    } else {
        if worktree.is_some() {
            task.worktree_path = None;
        }
        let window_closed = if cfg.cleanup.close_tmux_window || for_review { close_window(store, tmux, task) } else { false };
        log(
            EventLevel::Info,
            "cleanup.done",
            "cleanup finished (no worktree on disk)",
            serde_json::json!({ "succeeded": succeeded, "window_closed": window_closed }),
        )?;
    }
    Ok(())
}

/// Kill the latest session's tmux window; returns whether it happened.
fn close_window(store: &Store, tmux: &Tmux, task: &Task) -> bool {
    let session = match store.latest_session(task.id) {
        Ok(Some(s)) => s,
        Ok(None) => return false,
        Err(e) => {
            tracing::warn!(task = %task.key, error = %format!("{e:#}"), "cannot look up session for window cleanup");
            return false;
        }
    };
    match tmux.kill_window(&session.tmux_window) {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(task = %task.key, window = %session.tmux_window, error = %format!("{e:#}"), "window already gone or kill failed");
            false
        }
    }
}

/// Branch worktrees are compared against to count unpushed commits.
fn base_branch(cfg: &Config, repo: &Repo) -> Option<String> {
    cfg.repo.default_branch.clone().or_else(|| repo.default_branch().ok())
}

/// Environment for user cleanup commands.
fn cleanup_env(task: &Task, succeeded: bool) -> Vec<(String, String)> {
    vec![
        ("POWERQUEUE_TASK_ID".to_string(), task.id.to_string()),
        ("POWERQUEUE_TASK_KEY".to_string(), task.key.clone()),
        ("POWERQUEUE_TASK_SLUG".to_string(), task.slug()),
        ("POWERQUEUE_BRANCH".to_string(), task.branch.clone().unwrap_or_default()),
        ("POWERQUEUE_WORKTREE".to_string(), task.worktree_path.clone().unwrap_or_default()),
        ("POWERQUEUE_SUCCEEDED".to_string(), if succeeded { "1" } else { "0" }.to_string()),
    ]
}

/// Last `max` bytes of command output (on a char boundary).
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Worktree directory for a task under the configured root.
pub fn worktree_dir(root: &Path, task: &Task) -> std::path::PathBuf {
    root.join(task.slug())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Criticality, TaskSource, TaskState};
    use chrono::Duration;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn task(key: &str, state: TaskState, score: f64, crit: Criticality, age_mins: i64) -> Task {
        let mut t = Task::new(key, key, TaskSource::Manual);
        t.state = state;
        t.score = score;
        t.criticality = crit;
        t.created_at = now() - Duration::minutes(age_mins);
        t
    }

    #[test]
    fn pick_next_prefers_score_then_criticality_then_age() {
        let tasks = vec![
            task("a", TaskState::Queued, 100.0, Criticality::Normal, 10),
            task("b", TaskState::Queued, 500.0, Criticality::High, 5),
            task("c", TaskState::Queued, 500.0, Criticality::Critical, 1),
            task("d", TaskState::Queued, 500.0, Criticality::Critical, 30),
        ];
        assert_eq!(pick_next(&tasks, now()).unwrap().key, "d", "same score and criticality: oldest first");
        let tasks = vec![
            task("a", TaskState::Queued, 500.0, Criticality::Normal, 10),
            task("b", TaskState::Queued, 500.0, Criticality::High, 1),
        ];
        assert_eq!(pick_next(&tasks, now()).unwrap().key, "b", "same score: more critical first");
        let tasks = vec![
            task("a", TaskState::Queued, 900.0, Criticality::Low, 1),
            task("b", TaskState::Queued, 500.0, Criticality::Critical, 1),
        ];
        assert_eq!(pick_next(&tasks, now()).unwrap().key, "a", "score wins");
    }

    #[test]
    fn pick_next_skips_unschedulable_and_backoff() {
        let mut crashed = task("crashed", TaskState::Crashed, 999.0, Criticality::Critical, 1);
        crashed.not_before = Some(now() + Duration::minutes(1));
        let tasks = vec![
            crashed,
            task("running", TaskState::Running, 999.0, Criticality::Critical, 1),
            task("paused", TaskState::Paused, 999.0, Criticality::Critical, 1),
            task("done", TaskState::Completed, 999.0, Criticality::Critical, 1),
            task("attention", TaskState::NeedsAttention, 999.0, Criticality::Critical, 1),
            task("queued", TaskState::Queued, 1.0, Criticality::Low, 1),
        ];
        assert_eq!(pick_next(&tasks, now()).unwrap().key, "queued");
        assert_eq!(pick_next(&tasks, now() + Duration::minutes(2)).unwrap().key, "crashed", "backoff passed");
        let mut throttled = task("throttled", TaskState::Throttled, 5.0, Criticality::Low, 1);
        throttled.not_before = Some(now() - Duration::seconds(1));
        let tasks = vec![throttled, task("queued", TaskState::Queued, 1.0, Criticality::Low, 1)];
        assert_eq!(pick_next(&tasks, now()).unwrap().key, "throttled");
        assert!(pick_next(&[], now()).is_none());
    }

    #[test]
    fn pick_next_skips_tasks_waiting_on_dependencies() {
        let linked = |key: &str, state_type: &str| crate::domain::LinkedIssue {
            key: key.into(),
            title: String::new(),
            state_type: state_type.into(),
            pr_merged: false,
        };
        let mut blocked = task("blocked", TaskState::Queued, 999.0, Criticality::Critical, 1);
        blocked.blocked_by = vec![linked("B-1", "started")];
        let mut crashed = task("crashed", TaskState::Crashed, 998.0, Criticality::Critical, 1);
        crashed.blocked_by = vec![linked("B-1", "started")];
        let mut container = task("parent", TaskState::Queued, 997.0, Criticality::Critical, 1);
        container.children = vec![linked("C-1", "completed")];
        let mut tasks = vec![blocked, crashed, container, task("free", TaskState::Queued, 1.0, Criticality::Low, 1)];
        assert_eq!(pick_next(&tasks, now()).unwrap().key, "free");
        tasks[0].blocked_by[0].state_type = "completed".into();
        assert_eq!(pick_next(&tasks, now()).unwrap().key, "blocked", "a done blocker no longer holds it back");
    }

    fn cleanup_cfg() -> CleanupConfig {
        CleanupConfig::default()
    }

    #[test]
    fn plan_success_with_remote_pushes_and_removes() {
        let p = cleanup_plan(&cleanup_cfg(), true, true, 3, true);
        assert_eq!(
            p,
            CleanupPlan { push: true, remove_worktree: true, delete_branch: false, close_window: true, keep_reason: None }
        );
    }

    #[test]
    fn plan_keeps_failed_worktrees_by_default() {
        let p = cleanup_plan(&cleanup_cfg(), false, true, 0, true);
        assert!(!p.remove_worktree);
        assert!(p.keep_reason.as_deref().unwrap().contains("keep_failed"));
        let cfg = CleanupConfig { keep_failed: false, ..cleanup_cfg() };
        let p = cleanup_plan(&cfg, false, true, 0, true);
        assert!(p.remove_worktree);
    }

    #[test]
    fn plan_never_removes_unpushed_work() {
        let p = cleanup_plan(&cleanup_cfg(), true, true, 2, false);
        assert!(p.push);
        assert!(!p.remove_worktree);
        assert!(p.keep_reason.as_deref().unwrap().contains("push failed"));

        let p = cleanup_plan(&cleanup_cfg(), true, false, 2, false);
        assert!(!p.push);
        assert!(!p.remove_worktree);
        assert_eq!(p.keep_reason.as_deref(), Some("2 unpushed commit(s) and no remote to push to"));

        let cfg = CleanupConfig { push_branch: false, ..cleanup_cfg() };
        let p = cleanup_plan(&cfg, true, true, 1, false);
        assert!(!p.remove_worktree);
        assert_eq!(p.keep_reason.as_deref(), Some("1 unpushed commit(s) and cleanup.push_branch is false"));

        let p = cleanup_plan(&cfg, true, true, 0, false);
        assert!(p.remove_worktree, "nothing unpushed: safe to remove without a push");
    }

    #[test]
    fn plan_respects_remove_and_delete_flags() {
        let cfg = CleanupConfig { remove_worktree: false, delete_branch: true, close_tmux_window: false, ..cleanup_cfg() };
        let p = cleanup_plan(&cfg, true, true, 0, true);
        assert!(!p.remove_worktree);
        assert!(!p.delete_branch, "branch is only deleted when the worktree goes");
        assert!(!p.close_window);
        let cfg = CleanupConfig { delete_branch: true, ..cleanup_cfg() };
        let p = cleanup_plan(&cfg, true, true, 0, true);
        assert!(p.delete_branch);
    }

    #[test]
    fn env_and_tail_helpers() {
        let mut t = Task::new("ENG-9", "t", TaskSource::Manual);
        t.branch = Some("pq/eng-9".into());
        let env = cleanup_env(&t, true);
        assert!(env.contains(&("POWERQUEUE_TASK_KEY".to_string(), "ENG-9".to_string())));
        assert!(env.contains(&("POWERQUEUE_SUCCEEDED".to_string(), "1".to_string())));
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 3), "ab");
        assert_eq!(tail("aé", 1), "");
        assert_eq!(worktree_dir(Path::new("/w"), &t), std::path::PathBuf::from("/w/eng-9"));
    }
}
