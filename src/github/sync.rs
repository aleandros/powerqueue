//! Shared dry-run/apply reconciliation of execution approval and issue metadata.

use std::collections::HashSet;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;

use super::client::{GitHubClient, GitHubIssue};
use crate::config::{Config, GitHubConfig};
use crate::domain::{EventLevel, GitHubAdmission, Task, TaskSource, TaskState};
use crate::scheduler::transitions::{
    Effect, on_github_admission, on_github_closed, on_github_dependencies, on_github_source_admission, on_routing,
};
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
    pub problems: Vec<SyncProblem>,
}

/// One issue that could not be reconciled; other issues can still progress.
#[derive(Debug, Serialize)]
pub struct SyncProblem {
    pub key: String,
    pub reason: String,
}

fn issue_failure(plan: &mut SyncPlan, key: &str, error: anyhow::Error) -> Result<GitHubAdmission> {
    if super::client::source_failure(&error) {
        return Err(error);
    }
    let reason = format!("issue verification failed: {error:#}");
    plan.problems.push(SyncProblem { key: key.to_owned(), reason: reason.clone() });
    Ok(GitHubAdmission::denied(reason))
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
    task.title.clone_from(&issue.title);
    task.description = description;
    task.labels = labels;
    if let TaskSource::GitHub { url, .. } = &mut task.source {
        changed |= *url != issue.html_url;
        url.clone_from(&issue.html_url);
    }
    changed
}

/// Fetch intake and reconcile existing tasks from this repository. Terminal and
/// active tasks are never cancelled. Per-issue failures deny only that issue;
/// list, authentication and rate-limit failures abort without store writes.
pub async fn plan_sync(store: &Store, cfg: &GitHubConfig, client: &GitHubClient) -> Result<SyncPlan> {
    plan_sync_inner(store, cfg, client, None).await
}

/// Preview source intake with registry-validated deterministic routing.
/// Source-wide failures abort without writes; individual failures become holds.
pub async fn plan_sync_routed(store: &Store, cfg: &GitHubConfig, client: &GitHubClient, routing: &Config) -> Result<SyncPlan> {
    plan_sync_inner(store, cfg, client, Some(routing)).await
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "Keep the existing reconcile flag interface during lint adoption; callers already supply named state predicates."
)]
fn reconcile(
    task: &mut Task,
    admission: crate::domain::GitHubAdmission,
    source: &GitHubConfig,
    config: Option<&Config>,
    fresh: bool,
    live_session: bool,
    blockers: Option<Vec<crate::domain::LinkedIssue>>,
) {
    if let Some(blockers) = blockers {
        on_github_dependencies(task, blockers, live_session);
    }
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
    let mut plan = SyncPlan { execution_project: cfg.execution_project.clone(), fetched: issues.len(), ..Default::default() };
    let mut reconciled = HashSet::new();
    for issue in &issues {
        let mut fresh = task_from_issue(&cfg.repository, issue);
        fresh.execution_project = cfg.execution_project.clone();
        reconciled.insert(issue.number);
        let current = store.get_task_by_key(&fresh.key)?;
        if let Some(task) = &current
            && !matches!(&task.source, TaskSource::GitHub { repository, number, .. }
                if repository.eq_ignore_ascii_case(&cfg.repository) && *number == issue.number)
        {
            plan.problems.push(SyncProblem {
                key: fresh.key.clone(),
                reason: format!("source identity conflicts with existing task {}", task.id),
            });
            continue;
        }
        let (admission, blockers) = match client.execution_admission(cfg, issue).await {
            Ok(snapshot) => snapshot,
            Err(error) => (issue_failure(&mut plan, &fresh.key, error)?, None),
        };
        match current {
            None => {
                reconcile(&mut fresh, admission, cfg, routing, true, false, blockers);
                plan.changes.push(Change { action: "create", task: fresh });
            }
            Some(mut task)
                if !task.state.is_terminal()
                    && matches!(&task.source,
                TaskSource::GitHub { repository, number, .. } if repository.eq_ignore_ascii_case(&cfg.repository) && *number == issue.number) =>
            {
                let before = task.clone();
                update_issue(&mut task, issue);
                let live_session = store.latest_session(task.id)?.is_some_and(|s| s.state.is_live());
                reconcile(&mut task, admission, cfg, routing, false, live_session, blockers);
                if task != before {
                    plan.changes.push(Change { action: "update", task });
                }
            }
            Some(_) => {}
        }
    }
    for mut task in store.list_open_tasks()? {
        let TaskSource::GitHub { repository, number, .. } = &task.source else { continue };
        if !repository.eq_ignore_ascii_case(&cfg.repository) || reconciled.contains(number) {
            continue;
        }
        let before = task.clone();
        let (admission, blockers, closed) = match client.get_issue(repository, *number).await {
            Ok(issue) => {
                update_issue(&mut task, &issue);
                let (admission, blockers) = match client.execution_admission(cfg, &issue).await {
                    Ok(snapshot) => snapshot,
                    Err(error) => (issue_failure(&mut plan, &task.key, error)?, None),
                };
                (admission, blockers, issue.state == "closed" && issue.pull_request.is_none())
            }
            Err(error) => (issue_failure(&mut plan, &task.key, error)?, None, false),
        };
        let live_session = store.latest_session(task.id)?.is_some_and(|s| s.state.is_live());
        reconcile(&mut task, admission, cfg, routing, false, live_session, blockers);
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
    for problem in &plan.problems {
        record_problem(store, problem)?;
    }
    for change in &plan.changes {
        if !plan.problems.iter().any(|p| p.key == change.task.key) {
            store.kv_delete(&format!("github.sync_problem.{}", change.task.key))?;
        }
        let mut task = change.task.clone();
        let admission = task.github_admission.take();
        let routing = task.routing.take();
        let blockers = std::mem::take(&mut task.blocked_by);
        let effects;
        if change.action == "create" {
            if let Some(current) = store.get_task_by_key(&task.key)? {
                let same_source = matches!((&current.source, &task.source),
                    (TaskSource::GitHub { repository: a, number: an, .. }, TaskSource::GitHub { repository: b, number: bn, .. })
                    if a.eq_ignore_ascii_case(b) && an == bn);
                if !same_source || current.execution_project != task.execution_project {
                    record_problem(
                        store,
                        &SyncProblem {
                            key: task.key.clone(),
                            reason: "conflicting source or execution project acquired after preview; sync again".into(),
                        },
                    )?;
                }
                continue;
            }
            // Rebuild the initial state so application emits the transitions previewed above.
            let mut fresh = Task::new(&task.key, &task.title, task.source.clone());
            fresh.id = task.id;
            fresh.created_at = task.created_at;
            fresh.description = task.description;
            fresh.labels = task.labels;
            fresh.execution_project = task.execution_project;
            task = fresh;
            effects = apply_decisions(&mut task, admission, routing, &plan.execution_project, blockers, false);
            store.insert_task(&task).with_context(|| format!("create GitHub task {}", task.key))?;
        } else {
            let Some(mut current) = store.get_task(task.id)? else { continue };
            let before = current.clone();
            if current.state.is_terminal() {
                continue;
            }
            if change.action == "cancel" {
                if !(current.state.is_schedulable() || matches!(current.state, TaskState::Paused | TaskState::Blocked)) {
                    continue;
                }
                on_github_closed(&mut current, Utc::now());
            } else {
                current.title = task.title;
                current.description = task.description;
                current.labels = task.labels;
                current.source = task.source;
            }
            let live_session = store.latest_session(current.id)?.is_some_and(|s| s.state.is_live());
            effects = apply_decisions(&mut current, admission, routing, &plan.execution_project, blockers, live_session);
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
            serde_json::json!({"source": "github", "key": task.key, "execution_project": task.execution_project, "source_deferral": task.source_deferral().map_err(|e| e.to_string())}),
        )?;
        for effect in effects {
            if let Effect::Log { level, kind, message, data } = effect {
                store.log_event(Some(task.id), None, level, &kind, &message, data)?;
            }
        }
    }
    Ok(())
}

fn record_problem(store: &Store, problem: &SyncProblem) -> Result<()> {
    let key = format!("github.sync_problem.{}", problem.key);
    if store.kv_get::<String>(&key)?.as_ref() != Some(&problem.reason) {
        store.log_event(
            None,
            None,
            EventLevel::Warn,
            "github.issue_failed",
            &format!("{}: {}", problem.key, problem.reason),
            serde_json::to_value(problem)?,
        )?;
        store.kv_set(&key, &problem.reason)?;
    }
    Ok(())
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "Keep the existing apply_decisions flag interface during lint adoption; callers already supply named state predicates."
)]
fn apply_decisions(
    task: &mut Task,
    admission: Option<crate::domain::GitHubAdmission>,
    routing: Option<crate::routing::RoutingDecision>,
    fallback: &str,
    blockers: Vec<crate::domain::LinkedIssue>,
    live_session: bool,
) -> Vec<Effect> {
    let mut dependency_effects = on_github_dependencies(task, blockers, live_session);
    let effects = if let Some(mut routing) = routing {
        // The saved assignment may have changed since preview; compare again.
        routing.permitted = routing.project.is_some();
        let mut effects = on_routing(task, routing, false);
        if let Some(admission) = admission {
            effects.extend(on_github_admission(task, admission));
        }
        effects
    } else {
        admission.map(|a| on_github_source_admission(task, a, fallback)).unwrap_or_default()
    };
    dependency_effects.extend(effects);
    dependency_effects
}
