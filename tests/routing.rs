//! Routing across Linear metadata updates, database reopen and admission/start races.
use powerqueue::config::Config;
use powerqueue::domain::{Task, TaskSource, TaskState};
use powerqueue::linear::{LinearIssue, sync_issues_routed};
use powerqueue::scheduler::transitions::on_routing;
use powerqueue::store::Store;

#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
)]
fn config() -> Config {
    toml::from_str(
        r#"
        [repo]
        path = "/repos/default"
        [linear]
        execution_project = ""
        [projects.api.repo]
        path = "/repos/api"
        [projects.web.repo]
        path = "/repos/web"
        [[routing.rules]]
        name = "API team"
        project = "api"
        all = [{field="team",op="eq",value="API"}, {field="project",op="eq",value="Backend"}]
        [[routing.rules]]
        name = "Web label"
        project = "web"
        all = [{field="label",op="eq",value="repo/web"}]
    "#,
    )
    .unwrap()
}

fn issue() -> LinearIssue {
    LinearIssue {
        id: "linear-id".into(),
        identifier: "API-1".into(),
        title: "Backend work".into(),
        description: "A task".into(),
        url: "https://linear.app/API-1".into(),
        priority: 2,
        estimate: None,
        labels: vec![],
        state_name: "Todo".into(),
        state_type: "unstarted".into(),
        team_key: "API".into(),
        project: Some("Backend".into()),
        assignee_id: None,
        cycle: None,
        blocked_by: vec![],
        children: vec![],
        parent: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

#[test]
fn linear_metadata_changes_hold_retries_without_moving_the_checkout_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.sqlite");
    let store = Store::open(&path).unwrap();
    let cfg = config();
    let mut issue = issue();
    sync_issues_routed(&store, &cfg, &[issue.clone()], |_| Some("unstarted".into())).unwrap();
    let mut saved = store.get_task_by_key("API-1").unwrap().unwrap();
    assert_eq!(saved.execution_project, "api");
    assert!(saved.execution_approved());
    saved.state = TaskState::Crashed;
    saved.attempts = 1;
    saved.execution_repo = Some("/repos/api".into());
    saved.branch = Some("pq/api-1".into());
    store.update_task(&saved).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    issue.team_key = "WEB".into();
    issue.project = Some("Frontend".into());
    issue.labels.push("repo/web".into());
    sync_issues_routed(&store, &cfg, &[issue.clone()], |_| Some("unstarted".into())).unwrap();
    let changed = store.get_task(saved.id).unwrap().unwrap();
    assert_eq!(changed.execution_project, "api");
    assert_eq!(changed.execution_repo, saved.execution_repo);
    assert_eq!(changed.state, TaskState::Crashed);
    assert_eq!(changed.routing.as_ref().unwrap().project.as_deref(), Some("web"));
    assert!(!changed.execution_approved());
    assert!(store.set_execution_project(changed.id, "api", "web").is_err());
    issue.team_key = "API".into();
    issue.project = Some("Backend".into());
    issue.labels.clear();
    sync_issues_routed(&store, &cfg, &[issue], |_| Some("unstarted".into())).unwrap();
    assert!(store.get_task(saved.id).unwrap().unwrap().execution_approved());
    assert!(store.events_for_task(saved.id, 100).unwrap().iter().any(|e| e.kind == "routing.blocked"));
}

#[test]
fn unmatched_work_is_visible_and_requires_deliberate_assignment() {
    let store = Store::open_in_memory().unwrap();
    let cfg = config();
    let mut issue = issue();
    issue.project = None;
    sync_issues_routed(&store, &cfg, &[issue.clone()], |_| None).unwrap();
    let held = store.get_task_by_key("API-1").unwrap().unwrap();
    assert_eq!(held.execution_project, "");
    assert!(!held.execution_approved());
    assert!(held.routing.unwrap().reason.contains("no source fallback"));
    issue.project = Some("Backend".into());
    sync_issues_routed(&store, &cfg, &[issue.clone()], |_| None).unwrap();
    assert!(!store.get_task_by_key("API-1").unwrap().unwrap().execution_approved());
    store.set_execution_project(held.id, "", "api").unwrap();
    sync_issues_routed(&store, &cfg, &[issue], |_| None).unwrap();
    assert!(store.get_task(held.id).unwrap().unwrap().execution_approved());
}

#[test]
fn stale_launch_and_sync_cannot_overwrite_a_routing_hold_or_claim() {
    let store = Store::open_in_memory().unwrap();
    let cfg = config();
    let mut task = Task::new(
        "API-1",
        "work",
        TaskSource::Linear { issue_id: "id".into(), identifier: "API-1".into(), url: String::new(), team_key: "API".into() },
    );
    task.project = Some("Backend".into());
    let decision = cfg.route(&task);
    on_routing(&mut task, decision, true);
    store.insert_task(&task).unwrap();
    let mut starting = task.clone();
    starting.state = TaskState::Starting;
    starting.attempts = 1;
    let mut held = task.clone();
    held.labels.push("repo/web".into());
    let decision = cfg.route(&held);
    on_routing(&mut held, decision, false);
    assert!(store.update_source_task(&task, &held).unwrap());
    assert!(!store.claim_task_start(&task, &starting).unwrap());
    assert!(!store.record_routing(&task, &task).unwrap());
    let held = store.get_task(task.id).unwrap().unwrap();
    let mut restored = held.clone();
    restored.labels.clear();
    let decision = cfg.route(&restored);
    on_routing(&mut restored, decision, false);
    assert!(store.update_source_task(&held, &restored).unwrap());
    let restored = store.get_task(task.id).unwrap().unwrap();
    assert!(store.claim_task_start(&restored, &starting).unwrap());
    assert!(!store.update_source_task(&restored, &held).unwrap());
    assert_eq!(store.get_task(task.id).unwrap().unwrap().state, TaskState::Starting);
}

#[test]
fn schema_v8_upgrade_preserves_existing_execution_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.sqlite");
    let store = Store::open(&path).unwrap();
    let mut task = Task::new("legacy", "existing work", TaskSource::Manual);
    task.execution_project = "api".into();
    task.execution_repo = Some("/repos/api".into());
    task.state = TaskState::InReview;
    task.attempts = 2;
    store.insert_task(&task).unwrap();
    let task = store.get_task(task.id).unwrap().unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("ALTER TABLE tasks DROP COLUMN routing; PRAGMA user_version = 8;").unwrap();
    drop(conn);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), powerqueue::store::SCHEMA_VERSION);
    assert_eq!(store.get_task(task.id).unwrap().unwrap(), task);
    drop(store);
    assert_eq!(Store::open(&path).unwrap().get_task(task.id).unwrap().unwrap(), task);
}
