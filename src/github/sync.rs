//! Shared dry-run/apply reconciliation of execution approval and issue metadata.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Serialize;

use super::client::{GitHubClient, GitHubIssue};
use crate::config::{Config, GitHubConfig};
use crate::domain::{EventLevel, Task, TaskSource, TaskState};
use crate::scheduler::transitions::{Effect, on_github_admission, on_github_closed, on_github_source_admission, on_routing};
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
    pub execution_project: String,
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
/// the plan without changing the store. Approval revocation blocks future launches.
pub async fn plan_sync(store: &Store, cfg: &GitHubConfig, client: &GitHubClient) -> Result<SyncPlan> {
    plan_sync_inner(store, cfg, client, None).await
}

/// Preview source intake with registry-validated deterministic routing.
/// Fetch failures abort without writes; routing failures are visible held tasks.
pub async fn plan_sync_routed(store: &Store, cfg: &GitHubConfig, client: &GitHubClient, routing: &Config) -> Result<SyncPlan> {
    plan_sync_inner(store, cfg, client, Some(routing)).await
}

fn reconcile(
    task: &mut Task,
    admission: crate::domain::GitHubAdmission,
    source: &GitHubConfig,
    config: Option<&Config>,
    fresh: bool,
) {
    if let Some(config) = config {
        let decision = config.route(task);
        on_routing(task, decision, fresh);
        on_github_admission(task, admission);
    } else {
        on_github_source_admission(task, admission, &source.execution_project);
    }
}

async fn plan_sync_inner(store: &Store, cfg: &GitHubConfig, client: &GitHubClient, routing: Option<&Config>) -> Result<SyncPlan> {
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
    let mut plan = SyncPlan { execution_project: cfg.execution_project.clone(), fetched: issues.len(), changes: Vec::new() };
    let mut reconciled = HashSet::new();
    for issue in &issues {
        let mut fresh = task_from_issue(&cfg.repository, issue);
        fresh.execution_project = cfg.execution_project.clone();
        let admission = client.admission(cfg, issue).await?;
        reconciled.insert(issue.number);
        match store.get_task_by_key(&fresh.key)? {
            None => {
                reconcile(&mut fresh, admission, cfg, routing, true);
                plan.changes.push(Change { action: "create", task: fresh });
            }
            Some(mut task)
                if !task.state.is_terminal()
                    && matches!(&task.source,
                TaskSource::GitHub { repository, number, .. } if repository.eq_ignore_ascii_case(&cfg.repository) && *number == issue.number) =>
            {
                let before = task.clone();
                update_issue(&mut task, issue);
                reconcile(&mut task, admission, cfg, routing, false);
                if task != before {
                    plan.changes.push(Change { action: "update", task });
                }
            }
            Some(task) => {
                if !matches!(&task.source, TaskSource::GitHub { repository, number, .. }
                    if repository.eq_ignore_ascii_case(&cfg.repository) && *number == issue.number)
                {
                    bail!("GitHub source identity {} conflicts with existing task {}", fresh.key, task.id);
                }
            }
        }
    }
    for mut task in store.list_open_tasks()? {
        let TaskSource::GitHub { repository, number, .. } = &task.source else { continue };
        if !repository.eq_ignore_ascii_case(&cfg.repository) || reconciled.contains(number) {
            continue;
        }
        let issue = client.get_issue(repository, *number).await?;
        let before = task.clone();
        update_issue(&mut task, &issue);
        reconcile(&mut task, client.admission(cfg, &issue).await?, cfg, routing, false);
        let closed = issue.state == "closed" && issue.pull_request.is_none();
        if closed {
            on_github_closed(&mut task, Utc::now());
        }
        if task != before {
            plan.changes.push(Change { action: if task.state == TaskState::Cancelled { "cancel" } else { "update" }, task });
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
        let admission = task.github_admission.take();
        let routing = task.routing.take();
        let effects;
        if change.action == "create" {
            if let Some(current) = store.get_task_by_key(&task.key)? {
                let same_source = matches!((&current.source, &task.source),
                    (TaskSource::GitHub { repository: a, number: an, .. }, TaskSource::GitHub { repository: b, number: bn, .. })
                    if a.eq_ignore_ascii_case(b) && an == bn);
                if !same_source || current.execution_project != task.execution_project {
                    bail!(
                        "GitHub task {} acquired a conflicting source or execution project after preview; sync again",
                        task.key
                    );
                }
                continue;
            }
            effects = apply_decisions(&mut task, admission, routing, &plan.execution_project);
            store.insert_task(&task).with_context(|| format!("create GitHub task {}", task.key))?;
        } else {
            let Some(mut current) = store.get_task(task.id)? else { continue };
            let before = current.clone();
            if current.state.is_terminal() {
                continue;
            }
            if change.action == "cancel" {
                if !(current.state.is_schedulable() || current.state == TaskState::Paused) {
                    continue;
                }
                on_github_closed(&mut current, Utc::now());
            } else {
                current.title = task.title;
                current.description = task.description;
                current.labels = task.labels;
                current.source = task.source;
            }
            effects = apply_decisions(&mut current, admission, routing, &plan.execution_project);
            current.updated_at = Utc::now();
            if !store.update_source_task(&before, &current).with_context(|| format!("update GitHub task {}", current.key))? {
                continue;
            }
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
            serde_json::json!({"source": "github", "key": task.key, "execution_project": task.execution_project}),
        )?;
        for effect in effects {
            if let Effect::Log { level, kind, message, data } = effect {
                store.log_event(Some(task.id), None, level, &kind, &message, data)?;
            }
        }
    }
    Ok(())
}

fn apply_decisions(
    task: &mut Task,
    admission: Option<crate::domain::GitHubAdmission>,
    routing: Option<crate::routing::RoutingDecision>,
    fallback: &str,
) -> Vec<Effect> {
    if let Some(mut routing) = routing {
        // The saved assignment may have changed since preview; compare again.
        routing.permitted = routing.project.is_some();
        let mut effects = on_routing(task, routing, false);
        if let Some(admission) = admission {
            effects.extend(on_github_admission(task, admission));
        }
        effects
    } else {
        admission.map(|a| on_github_source_admission(task, a, fallback)).unwrap_or_default()
    }
}
