//! Merge fetched Linear issues into the task store.

use std::collections::HashSet;

use anyhow::{Context, Result};
use chrono::Utc;
use tracing::{debug, info};

use crate::config::LinearConfig;
use crate::domain::{EventLevel, Task, TaskId, TaskSource, TaskState};
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

impl SyncReport {
    /// True if nothing was created, updated or cancelled.
    pub fn is_noop(&self) -> bool {
        self.created.is_empty() && self.updated.is_empty() && self.cancelled.is_empty()
    }
}

/// Message stored in `last_error` when sync cancels a task because the issue was closed.
pub const CLOSED_IN_LINEAR: &str = "closed in Linear";

/// Reconcile `issues` (the current queued set from Linear) with stored tasks:
///
/// * unknown issue → new `Queued` task (key = identifier);
/// * known open task whose issue changed → update fields (not state);
/// * known open task (queued/throttled/paused/crashed) whose issue is absent
///   from `issues` and, per `fetch_state`, is completed/cancelled in Linear →
///   `Cancelled`. Running tasks are never cancelled by sync; the daemon
///   decides what to do with them.
///
/// `fetch_state(issue_id)` returns the issue's current workflow state *type*
/// (`completed`, `canceled`, `started`, ...) or `None` if the issue no longer
/// exists; it is only called for tasks whose issue is missing from `issues`.
/// Tasks that are already terminal are left alone. Every change is recorded
/// with `store.log_event` (`task.created`, `task.updated`, `task.cancelled`).
pub fn sync_issues(
    store: &Store,
    cfg: &LinearConfig,
    issues: &[LinearIssue],
    fetch_state: impl Fn(&str) -> Option<String>,
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    debug!(target: "powerqueue::linear", issues = issues.len(), teams = ?cfg.team_keys, "sync start");

    let mut seen: HashSet<&str> = HashSet::with_capacity(issues.len());
    for issue in issues {
        seen.insert(issue.id.as_str());
        match store.get_task_by_linear_issue(&issue.id)? {
            None => {
                // Guard against a key collision with a manual task or a task
                // re-created under the same identifier.
                if let Some(existing) = store.get_task_by_key(&issue.identifier)? {
                    debug!(target: "powerqueue::linear", task = %existing.key, "key already used by a non-Linear task; skipping");
                    report.unchanged += 1;
                    continue;
                }
                let task = task_from_issue(issue);
                store.insert_task(&task).with_context(|| format!("create task for {}", issue.identifier))?;
                store.log_event(
                    Some(task.id),
                    None,
                    EventLevel::Info,
                    "task.created",
                    &format!("created from Linear issue {} ({})", issue.identifier, issue.state_name),
                    serde_json::json!({ "identifier": issue.identifier, "url": issue.url, "team": issue.team_key }),
                )?;
                info!(target: "powerqueue::linear", task = %task.key, "task created from Linear");
                report.created.push(task.id);
            }
            Some(existing) if existing.state.is_terminal() => {
                debug!(target: "powerqueue::linear", task = %existing.key, state = %existing.state, "terminal task; left alone");
            }
            Some(mut existing) => {
                let changed = apply_issue(&mut existing, issue);
                if changed.is_empty() {
                    report.unchanged += 1;
                    continue;
                }
                existing.updated_at = Utc::now();
                store.update_task(&existing).with_context(|| format!("update task {}", existing.key))?;
                store.log_event(
                    Some(existing.id),
                    None,
                    EventLevel::Debug,
                    "task.updated",
                    &format!("Linear issue changed: {}", changed.join(", ")),
                    serde_json::json!({ "fields": changed }),
                )?;
                debug!(target: "powerqueue::linear", task = %existing.key, fields = ?changed, "task updated from Linear");
                report.updated.push(existing.id);
            }
        }
    }

    for mut task in store.list_open_tasks()? {
        let Some(issue_id) = task.linear_issue_id().map(str::to_string) else { continue };
        if seen.contains(issue_id.as_str()) {
            continue;
        }
        if !(task.state.is_schedulable() || task.state == TaskState::Paused) {
            // Running / idle / needs-attention: the daemon owns these.
            continue;
        }
        let state_type = fetch_state(&issue_id);
        let closed = match state_type.as_deref().map(str::to_ascii_lowercase).as_deref() {
            None => true,
            Some("completed") | Some("canceled") | Some("cancelled") => true,
            Some(_) => false,
        };
        if !closed {
            debug!(target: "powerqueue::linear", task = %task.key, state = ?state_type, "issue left the queue but is still open");
            continue;
        }
        let why = match &state_type {
            Some(t) => format!("{CLOSED_IN_LINEAR} ({t})"),
            None => format!("{CLOSED_IN_LINEAR} (issue deleted)"),
        };
        let previous = task.state;
        task.state = TaskState::Cancelled;
        task.last_error = Some(CLOSED_IN_LINEAR.to_string());
        task.completed_at = Some(Utc::now());
        task.updated_at = Utc::now();
        store.update_task(&task).with_context(|| format!("cancel task {}", task.key))?;
        store.log_event(
            Some(task.id),
            None,
            EventLevel::Warn,
            "task.cancelled",
            &why,
            serde_json::json!({ "previous_state": previous.as_str(), "linear_state_type": state_type }),
        )?;
        info!(target: "powerqueue::linear", task = %task.key, previous = %previous, "task cancelled: {why}");
        report.cancelled.push(task.id);
    }

    debug!(
        target: "powerqueue::linear",
        created = report.created.len(),
        updated = report.updated.len(),
        cancelled = report.cancelled.len(),
        unchanged = report.unchanged,
        "sync done"
    );
    Ok(report)
}

/// Build a fresh queued task from an issue.
pub fn task_from_issue(issue: &LinearIssue) -> Task {
    let mut task = Task::new(
        issue.identifier.clone(),
        issue.title.clone(),
        TaskSource::Linear {
            issue_id: issue.id.clone(),
            identifier: issue.identifier.clone(),
            url: issue.url.clone(),
            team_key: issue.team_key.clone(),
        },
    );
    task.description = issue.description.clone();
    task.labels = issue.labels.clone();
    task.linear_priority = Some(issue.priority);
    task.estimate = issue.estimate;
    task.project = issue.project.clone();
    task.created_at = issue.created_at;
    task
}

/// Copy the mutable issue fields onto an existing task. Returns the names of
/// the fields that changed (empty = nothing to persist). State, score and
/// scoring reasons are untouched; the scheduler re-evaluates rules itself.
pub fn apply_issue(task: &mut Task, issue: &LinearIssue) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if task.title != issue.title {
        task.title = issue.title.clone();
        changed.push("title");
    }
    if task.description != issue.description {
        task.description = issue.description.clone();
        changed.push("description");
    }
    if task.labels != issue.labels {
        task.labels = issue.labels.clone();
        changed.push("labels");
    }
    if task.linear_priority != Some(issue.priority) {
        task.linear_priority = Some(issue.priority);
        changed.push("priority");
    }
    if task.estimate != issue.estimate {
        task.estimate = issue.estimate;
        changed.push("estimate");
    }
    if task.project != issue.project {
        task.project = issue.project.clone();
        changed.push("project");
    }
    let source = TaskSource::Linear {
        issue_id: issue.id.clone(),
        identifier: issue.identifier.clone(),
        url: issue.url.clone(),
        team_key: issue.team_key.clone(),
    };
    if task.source != source {
        task.source = source;
        changed.push("source");
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Criticality;

    fn issue(id: &str, ident: &str, title: &str) -> LinearIssue {
        LinearIssue {
            id: id.into(),
            identifier: ident.into(),
            title: title.into(),
            description: "desc".into(),
            url: format!("https://linear.app/x/{ident}"),
            priority: 2,
            estimate: Some(3.0),
            labels: vec!["bug".into()],
            state_name: "Todo".into(),
            state_type: "unstarted".into(),
            team_key: "ENG".into(),
            project: Some("Launch".into()),
            assignee_id: None,
            created_at: Utc::now() - chrono::Duration::hours(5),
            updated_at: Utc::now(),
        }
    }

    fn no_state(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn creates_tasks_for_new_issues() {
        let store = Store::open_in_memory().unwrap();
        let cfg = LinearConfig::default();
        let issues = vec![issue("u1", "ENG-1", "One"), issue("u2", "ENG-2", "Two")];
        let report = sync_issues(&store, &cfg, &issues, no_state).unwrap();
        assert_eq!(report.created.len(), 2);
        assert!(report.updated.is_empty() && report.cancelled.is_empty());
        let t = store.get_task_by_key("ENG-1").unwrap().unwrap();
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.description, "desc");
        assert_eq!(t.linear_priority, Some(2));
        assert_eq!(t.estimate, Some(3.0));
        assert_eq!(t.project.as_deref(), Some("Launch"));
        assert_eq!(t.labels, vec!["bug".to_string()]);
        assert!(matches!(&t.source, TaskSource::Linear { team_key, .. } if team_key == "ENG"));
        let events = store.events_for_task(t.id, 10).unwrap();
        assert!(events.iter().any(|e| e.kind == "task.created"));

        // Second pass: nothing changes.
        let report = sync_issues(&store, &cfg, &issues, no_state).unwrap();
        assert!(report.is_noop());
        assert_eq!(report.unchanged, 2);
    }

    #[test]
    fn updates_fields_but_not_state_or_scoring() {
        let store = Store::open_in_memory().unwrap();
        let cfg = LinearConfig::default();
        let mut issues = vec![issue("u1", "ENG-1", "One")];
        sync_issues(&store, &cfg, &issues, no_state).unwrap();
        let mut t = store.get_task_by_key("ENG-1").unwrap().unwrap();
        t.state = TaskState::Running;
        t.criticality = Criticality::Critical;
        t.score = 1234.0;
        t.score_reasons = vec!["kept".into()];
        store.update_task(&t).unwrap();

        issues[0].title = "One (renamed)".into();
        issues[0].labels = vec!["bug".into(), "customer".into()];
        let report = sync_issues(&store, &cfg, &issues, no_state).unwrap();
        assert_eq!(report.updated, vec![t.id]);
        let t2 = store.get_task_by_key("ENG-1").unwrap().unwrap();
        assert_eq!(t2.title, "One (renamed)");
        assert_eq!(t2.labels.len(), 2);
        assert_eq!(t2.state, TaskState::Running);
        assert_eq!(t2.criticality, Criticality::Critical);
        assert_eq!(t2.score, 1234.0);
        assert_eq!(t2.score_reasons, vec!["kept".to_string()]);
    }

    #[test]
    fn cancels_queued_tasks_closed_in_linear() {
        let store = Store::open_in_memory().unwrap();
        let cfg = LinearConfig::default();
        let issues = vec![issue("u1", "ENG-1", "One"), issue("u2", "ENG-2", "Two"), issue("u3", "ENG-3", "Three")];
        sync_issues(&store, &cfg, &issues, no_state).unwrap();
        let mut running = store.get_task_by_key("ENG-3").unwrap().unwrap();
        running.state = TaskState::Running;
        store.update_task(&running).unwrap();

        // ENG-1 completed, ENG-2 moved to In Progress by a human, ENG-3 (running) deleted.
        let fetch = |id: &str| match id {
            "u1" => Some("completed".to_string()),
            "u2" => Some("started".to_string()),
            _ => None,
        };
        let report = sync_issues(&store, &cfg, &[], fetch).unwrap();
        let one = store.get_task_by_key("ENG-1").unwrap().unwrap();
        assert_eq!(report.cancelled, vec![one.id]);
        assert_eq!(one.state, TaskState::Cancelled);
        assert_eq!(one.last_error.as_deref(), Some(CLOSED_IN_LINEAR));
        assert!(store.events_for_task(one.id, 10).unwrap().iter().any(|e| e.kind == "task.cancelled"));
        assert_eq!(store.get_task_by_key("ENG-2").unwrap().unwrap().state, TaskState::Queued);
        assert_eq!(store.get_task_by_key("ENG-3").unwrap().unwrap().state, TaskState::Running);

        // A deleted issue (None) also cancels, and terminal tasks are left alone afterwards.
        let report = sync_issues(&store, &cfg, &[], |_| None).unwrap();
        let two = store.get_task_by_key("ENG-2").unwrap().unwrap();
        assert_eq!(report.cancelled, vec![two.id]);
        let report = sync_issues(&store, &cfg, &[], |_| None).unwrap();
        assert!(report.cancelled.is_empty());
    }

    #[test]
    fn terminal_tasks_are_left_alone_when_issue_reappears() {
        let store = Store::open_in_memory().unwrap();
        let cfg = LinearConfig::default();
        let issues = vec![issue("u1", "ENG-1", "One")];
        sync_issues(&store, &cfg, &issues, no_state).unwrap();
        let mut t = store.get_task_by_key("ENG-1").unwrap().unwrap();
        t.state = TaskState::Completed;
        store.update_task(&t).unwrap();
        let report = sync_issues(&store, &cfg, &issues, no_state).unwrap();
        assert!(report.is_noop());
        assert_eq!(store.get_task_by_key("ENG-1").unwrap().unwrap().state, TaskState::Completed);
    }

    #[test]
    fn paused_tasks_are_cancelled_when_closed() {
        let store = Store::open_in_memory().unwrap();
        let cfg = LinearConfig::default();
        sync_issues(&store, &cfg, &[issue("u1", "ENG-1", "One")], no_state).unwrap();
        let mut t = store.get_task_by_key("ENG-1").unwrap().unwrap();
        t.state = TaskState::Paused;
        store.update_task(&t).unwrap();
        let report = sync_issues(&store, &cfg, &[], |_| Some("canceled".into())).unwrap();
        assert_eq!(report.cancelled.len(), 1);
    }
}
