//! Shared dry-run/apply reconciliation. Only confirmed closures cancel tasks.

use std::collections::HashSet;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;

use super::client::{GitHubClient, GitHubIssue};
use crate::config::GitHubConfig;
use crate::domain::{EventLevel, Task, TaskSource, TaskState};
use crate::store::Store;

/// One planned local queue change. The task is the complete updated snapshot.
#[derive(Debug, Serialize)]
pub struct Change {
    pub action: &'static str,
    pub task: Task,
}

/// A preview which can be applied without fetching a different remote snapshot.
#[derive(Debug, Default, Serialize)]
pub struct SyncPlan {
    pub fetched: usize,
    pub changes: Vec<Change>,
}

/// Construct a fresh queued task from a GitHub issue.
pub fn task_from_issue(repository: &str, issue: &GitHubIssue) -> Task {
    let mut task = Task::new(
        format!("{repository}#{}", issue.number),
        &issue.title,
        TaskSource::GitHub { repository: repository.to_string(), number: issue.number, url: issue.html_url.clone() },
    );
    task.description = issue.body.clone().unwrap_or_default();
    task.labels = issue.labels.iter().map(|l| l.name.clone()).collect();
    task.created_at = issue.created_at;
    task
}

fn update_issue(task: &mut Task, issue: &GitHubIssue) -> bool {
    let description = issue.body.clone().unwrap_or_default();
    let labels: Vec<_> = issue.labels.iter().map(|l| l.name.clone()).collect();
    let mut changed = task.title != issue.title || task.description != description || task.labels != labels;
    task.title = issue.title.clone();
    task.description = description;
    task.labels = labels;
    if let TaskSource::GitHub { url, .. } = &mut task.source {
        changed |= *url != issue.html_url;
        *url = issue.html_url.clone();
    }
    changed
}

/// Fetch intake and reconcile existing tasks from this repository. Terminal and
/// active tasks are never cancelled. Missing access or any failed request aborts
/// the plan without changing the store. Tasks outside intake remain queued while open.
pub async fn plan_sync(store: &Store, cfg: &GitHubConfig, client: &GitHubClient) -> Result<SyncPlan> {
    let finished: HashSet<_> = store
        .list_tasks()?
        .into_iter()
        .filter_map(|t| match t.source {
            TaskSource::GitHub { repository, number, .. }
                if t.state.is_terminal() && repository.eq_ignore_ascii_case(&cfg.repository) =>
            {
                Some(number)
            }
            _ => None,
        })
        .collect();
    let issues = client.fetch_issues_ignoring(cfg, &finished).await?;
    let mut plan = SyncPlan { fetched: issues.len(), changes: Vec::new() };
    for issue in &issues {
        let fresh = task_from_issue(&cfg.repository, issue);
        match store.get_task_by_key(&fresh.key)? {
            None => plan.changes.push(Change { action: "create", task: fresh }),
            Some(mut task)
                if !task.state.is_terminal()
                    && matches!(&task.source,
                TaskSource::GitHub { repository, number, .. } if repository.eq_ignore_ascii_case(&cfg.repository) && *number == issue.number) =>
            {
                if update_issue(&mut task, issue) {
                    task.updated_at = Utc::now();
                    plan.changes.push(Change { action: "update", task });
                }
            }
            _ => {}
        }
    }
    for mut task in store.list_open_tasks()? {
        let TaskSource::GitHub { repository, number, .. } = &task.source else { continue };
        if !repository.eq_ignore_ascii_case(&cfg.repository)
            || issues.iter().any(|i| i.number == *number)
            || !(task.state.is_schedulable() || task.state == TaskState::Paused)
        {
            continue;
        }
        let issue = client.get_issue(repository, *number).await?;
        if issue.state == "closed" && issue.pull_request.is_none() {
            task.state = TaskState::Cancelled;
            task.last_error = Some("closed in GitHub".into());
            task.completed_at = Some(Utc::now());
            task.updated_at = Utc::now();
            plan.changes.push(Change { action: "cancel", task });
        }
    }
    Ok(plan)
}

/// Apply a preview and log each state change. Fails on persistence errors.
/// Refresh mutable metadata on the latest task so a concurrent daemon/CLI change
/// cannot overwrite execution state with the preview's snapshot.
pub fn apply_plan(store: &Store, plan: &SyncPlan) -> Result<()> {
    for change in &plan.changes {
        let mut task = change.task.clone();
        if change.action == "create" {
            if store.get_task_by_key(&task.key)?.is_some() {
                continue;
            }
            store.insert_task(&task).with_context(|| format!("create GitHub task {}", task.key))?;
        } else {
            let Some(mut current) = store.get_task(task.id)? else { continue };
            if current.state.is_terminal() {
                continue;
            }
            if change.action == "cancel" {
                if !(current.state.is_schedulable() || current.state == TaskState::Paused) {
                    continue;
                }
                current.state = task.state;
                current.last_error = task.last_error;
                current.completed_at = task.completed_at;
            } else {
                current.title = task.title;
                current.description = task.description;
                current.labels = task.labels;
                current.source = task.source;
            }
            current.updated_at = Utc::now();
            store.update_task(&current).with_context(|| format!("update GitHub task {}", current.key))?;
            task = current;
        }
        let kind = match change.action {
            "create" => "task.created",
            "cancel" => "task.cancelled",
            _ => "task.updated",
        };
        store.log_event(
            Some(task.id),
            None,
            EventLevel::Info,
            kind,
            &format!("GitHub sync: {} {}", change.action, task.key),
            serde_json::json!({"source": "github", "key": task.key}),
        )?;
    }
    Ok(())
}
