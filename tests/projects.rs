//! Execution registry persistence and migration across process restarts.
use powerqueue::domain::{Task, TaskSource};
use powerqueue::store::{SCHEMA_VERSION, Store};

#[test]
fn v6_tasks_migrate_to_default_and_project_assignment_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("queue.sqlite");
    let mut task = Task::new("legacy", "legacy task", TaskSource::Manual);
    task.project = Some("Independent Linear project".into());
    {
        let store = Store::open(&db).unwrap();
        store.insert_task(&task).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("ALTER TABLE tasks DROP COLUMN execution_project; ALTER TABLE tasks DROP COLUMN execution_repo; PRAGMA user_version = 6;").unwrap();
    }
    for _ in 0..2 {
        let store = Store::open(&db).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        let migrated = store.get_task(task.id).unwrap().unwrap();
        assert_eq!(migrated.execution_project, "default");
        assert_eq!(migrated.execution_repo, None);
        assert_eq!(migrated.project, task.project);
    }
    {
        let store = Store::open(&db).unwrap();
        task.execution_project = "api".into();
        task.execution_repo = Some("/checkouts/api".into());
        store.update_task(&task).unwrap();
    }
    let store = Store::open(&db).unwrap();
    let restored = store.get_task(task.id).unwrap().unwrap();
    assert_eq!(restored.execution_project, "api");
    assert_eq!(restored.execution_repo.as_deref(), Some("/checkouts/api"));
    assert_eq!(restored.project, task.project);
}

#[test]
fn legacy_task_json_defaults_to_the_legacy_execution_project() {
    let task = Task::new("legacy", "old JSON", TaskSource::Manual);
    let mut value = serde_json::to_value(&task).unwrap();
    value.as_object_mut().unwrap().remove("execution_project");
    value.as_object_mut().unwrap().remove("execution_repo");
    let restored: Task = serde_json::from_value(value).unwrap();
    assert_eq!(restored, task);
}

#[test]
fn worktree_ownership_and_pinned_checkout_prevent_cross_repository_operations() {
    if which::which("git").is_err() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let make_repo = |name: &str| {
        let path = tmp.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        let repo = powerqueue::worktree::Repo::new(&path);
        repo.git(None, &["init", "-q", "-b", "main"]).unwrap();
        repo.git(None, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--allow-empty", "-qm", "init"]).unwrap();
        repo
    };
    let a = make_repo("a");
    let b = make_repo("b");
    let worktree = tmp.path().join("worktree-b");
    b.add_worktree_on(&worktree, "pq/task", Some("main")).unwrap();
    assert!(a.ensure_worktree_owner(&worktree).is_err());
    b.ensure_worktree_owner(&worktree).unwrap();
    let mut cfg = powerqueue::config::Config::default();
    cfg.repo.path = a.path.display().to_string();
    let mut project = powerqueue::projects::ProjectConfig::default();
    project.repo.path = b.path.display().to_string();
    cfg.projects.insert("api".into(), project);
    let paths = powerqueue::paths::Paths::rooted(tmp.path());
    let mut task = Task::new("T", "test", TaskSource::Manual);
    task.execution_project = "api".into();
    task.execution_repo = Some(b.path.canonicalize().unwrap().display().to_string());
    task.worktree_path = Some(worktree.display().to_string());
    cfg.for_task(&task, &paths).unwrap();
    cfg.projects.get_mut("api").unwrap().repo.path = a.path.display().to_string();
    assert!(cfg.for_task(&task, &paths).unwrap_err().to_string().contains("pinned"));
    task.execution_repo = None; // Legacy rows without a pin are still protected by ownership.
    assert!(cfg.for_task(&task, &paths).unwrap_err().to_string().contains("another repository"));
    assert!(b.branch_exists("pq/task").unwrap());
    assert!(!a.branch_exists("pq/task").unwrap());
    assert!(worktree.exists());
}

#[test]
fn cli_project_assignment_validates_target_and_keeps_source_metadata() {
    use assert_cmd::Command;
    let tmp = tempfile::tempdir().unwrap();
    let paths = powerqueue::paths::Paths::rooted(tmp.path());
    paths.ensure().unwrap();
    let mut cfg = powerqueue::config::Config::default();
    cfg.repo.path = tmp.path().display().to_string();
    let mut project = powerqueue::projects::ProjectConfig::default();
    project.repo.path = tmp.path().display().to_string();
    cfg.projects.insert("api".into(), project);
    cfg.save(&paths).unwrap();
    let cmd = || {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("powerqueue"));
        cmd.env("POWERQUEUE_HOME", tmp.path()).env("POWERQUEUE_SECRETS", "file");
        cmd
    };
    cmd().args(["add", "test", "--key", "T", "--project", "api"]).assert().success();
    cmd().args(["task", "project", "T", "missing"]).assert().failure();
    let store = Store::open(&paths.database()).unwrap();
    let task = store.get_task_by_key("T").unwrap().unwrap();
    assert_eq!(task.execution_project, "api");
    let output = cmd().args(["--json", "task", "project", "T", "default"]).assert().success().get_output().stdout.clone();
    let response: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(response["execution_project"], "default");
    assert_eq!(response["queued"], false);
    let mut task = store.get_task(task.id).unwrap().unwrap();
    assert_eq!(task.execution_project, "default");
    assert_eq!(task.source, TaskSource::Manual);
    assert!(store.events_for_task(task.id, 10).unwrap().iter().any(|e| e.kind == "task.project_set"));
    task.execution_repo = Some(tmp.path().display().to_string());
    store.update_task(&task).unwrap();
    cmd().args(["task", "project", "T", "api"]).assert().failure();
    assert_eq!(store.get_task(task.id).unwrap().unwrap().execution_project, "default");
}

#[test]
fn reassignment_and_launch_claim_cannot_cross_each_other() {
    let store = Store::open_in_memory().unwrap();
    let task = Task::new("race", "project race", TaskSource::Manual);
    store.insert_task(&task).unwrap();
    let mut starting = task.clone();
    starting.state = powerqueue::domain::TaskState::Starting;
    starting.model = Some(powerqueue::domain::ModelTier::sonnet());
    store.set_execution_project(task.id, "default", "api").unwrap();
    assert!(!store.claim_task_start(&task, &starting).unwrap(), "stale default-project candidate cannot start");
    let current = store.get_task(task.id).unwrap().unwrap();
    starting.execution_project = "api".into();
    assert!(store.claim_task_start(&current, &starting).unwrap());
    assert!(store.set_execution_project(task.id, "api", "default").is_err(), "a claimed task cannot be reassigned");
    let stored = store.get_task(task.id).unwrap().unwrap();
    assert_eq!(stored.execution_project, "api");
    assert_eq!(stored.state, powerqueue::domain::TaskState::Starting);
}
