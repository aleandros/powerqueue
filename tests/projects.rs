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

#[test]
fn project_setup_reconfiguration_and_filtered_views() {
    use assert_cmd::Command;
    use predicates::str::contains;
    if which::which("git").is_err() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let paths = powerqueue::paths::Paths::rooted(&home);
    let cmd = || {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("powerqueue"));
        cmd.env("POWERQUEUE_HOME", &home).env("POWERQUEUE_SECRETS", "file").env("NO_COLOR", "1");
        cmd
    };
    let mut repos = Vec::new();
    for id in ["api", "web", "worker"] {
        let repo = tmp.path().join(id);
        std::fs::create_dir_all(&repo).unwrap();
        assert!(std::process::Command::new("git").args(["init", "-q", "-b", "main"]).arg(&repo).status().unwrap().success());
        cmd()
            .args([
                "project",
                "register",
                id,
                repo.to_str().unwrap(),
                "--github",
                &format!("example/{id}"),
                "--token-env",
                "FIXTURE_TOKEN",
                "--trusted-approver",
                "maintainer",
            ])
            .assert()
            .success();
        cmd().args(["add", &format!("Task in {id}"), "--project", id, "--key", id]).assert().success();
        repos.push(repo);
    }
    let mut text = std::fs::read_to_string(paths.config_file()).unwrap();
    let initial = powerqueue::config::Config::from_toml(&text).unwrap();
    assert_eq!(initial.github_sources[0].required_labels, ["agent:ready"]);
    text = text.replacen("required_labels = [\"agent:ready\"]", "required_labels = [\"approved-work\"]", 1).replacen(
        "enabled = true",
        "enabled = false",
        1,
    );
    text.push_str(
        "\n# preserve this machine setting\n[scheduler]\nmax_concurrent = 3\n[projects.api.cleanup]\npush_branch = false\n",
    );
    std::fs::write(paths.config_file(), &text).unwrap();
    cmd().args(["project", "register", "api", repos[0].to_str().unwrap(), "--github", "EXAMPLE/API"]).assert().success();
    let updated = std::fs::read_to_string(paths.config_file()).unwrap();
    assert!(updated.contains("# preserve this machine setting"));
    let cfg = powerqueue::config::Config::from_toml(&updated).unwrap();
    assert_eq!(cfg.scheduler.max_concurrent, 3);
    assert_eq!(cfg.github_sources.len(), 3);
    assert_eq!(cfg.github_sources[0].required_labels, ["approved-work"]);
    assert!(!cfg.github_sources[0].enabled);
    assert_eq!(cfg.github_sources[0].trusted_approvers, ["maintainer"]);
    assert_eq!(cfg.github_sources[0].token_env.as_deref(), Some("FIXTURE_TOKEN"));
    assert!(!cfg.projects["api"].cleanup.as_ref().unwrap().push_branch);
    cmd()
        .args(["project", "register", "worker", repos[2].to_str().unwrap(), "--github", "example/api"])
        .assert()
        .failure()
        .stderr(contains("already maps"));
    cmd().args(["project", "register", "../bad", repos[0].to_str().unwrap()]).assert().failure();
    cmd().args(["project", "register", "api", tmp.path().to_str().unwrap()]).assert().failure();
    assert_eq!(std::fs::read_to_string(paths.config_file()).unwrap(), updated, "rejected edits leave config intact");
    let out = cmd().args(["project", "list", "--json"]).assert().success().get_output().stdout.clone();
    let list: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(list["projects"].as_array().unwrap().len(), 3);
    assert_eq!(list["max_concurrent"], 3);
    for command in [
        vec!["status", "--project", "web"],
        vec!["task", "list", "--project", "web"],
        vec!["dashboard", "--once", "--project", "web"],
    ] {
        let out = cmd().arg("--json").args(&command).assert().success().get_output().stdout.clone();
        let view: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(view["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(view["tasks"][0]["execution_project"], "web");
        assert_eq!(view["project_filter"], "web");
    }
    cmd().args(["status"]).assert().success().stdout(contains("PROJECT")).stdout(contains("worker"));
    cmd()
        .args(["dashboard", "--once", "--ascii", "--project", "worker"])
        .assert()
        .success()
        .stdout(contains("PROJECT"))
        .stdout(contains("worker"));
    cmd().args(["task", "explain", "api"]).assert().success().stdout(contains("project      api"));
    for operation in ["show", "explain"] {
        let out = cmd().args(["--json", "task", operation, "api"]).assert().success().get_output().stdout.clone();
        let view: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(view["execution_location"]["checkout"], repos[0].canonicalize().unwrap().display().to_string());
    }
    // A pinned task prevents checkout redirection during reconfiguration.
    let store = Store::open(&paths.database()).unwrap();
    let mut task = store.get_task_by_key("api").unwrap().unwrap();
    task.execution_repo = Some(repos[0].canonicalize().unwrap().display().to_string());
    store.update_task(&task).unwrap();
    cmd().args(["project", "register", "api", repos[1].to_str().unwrap()]).assert().failure().stderr(contains("pinned"));
    assert_eq!(std::fs::read_to_string(paths.config_file()).unwrap(), updated);
}

#[test]
fn project_registration_preserves_legacy_default_and_explicit_approval() {
    use assert_cmd::Command;
    if which::which("git").is_err() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    assert!(std::process::Command::new("git").args(["init", "-q"]).arg(&repo).status().unwrap().success());
    let paths = powerqueue::paths::Paths::rooted(&tmp.path().join("home"));
    paths.ensure().unwrap();
    let original = format!(
        r#"# existing installation
[repo]
path = {:?}
setup = ["echo preserved"]
[github]
repository = "example/legacy"
enabled = false
required_labels = ["powerqueue"]
"#,
        repo.display().to_string()
    );
    std::fs::write(paths.config_file(), original).unwrap();
    Command::new(assert_cmd::cargo::cargo_bin!("powerqueue"))
        .env("POWERQUEUE_HOME", tmp.path().join("home"))
        .env("POWERQUEUE_SECRETS", "file")
        .args(["project", "register", "default", repo.to_str().unwrap(), "--github", "example/legacy"])
        .assert()
        .success();
    let text = std::fs::read_to_string(paths.config_file()).unwrap();
    assert!(text.contains("# existing installation"));
    let cfg = powerqueue::config::Config::from_toml(&text).unwrap();
    assert!(cfg.projects.is_empty() && cfg.github_sources.is_empty());
    assert_eq!(cfg.repo.setup, ["echo preserved"]);
    assert_eq!(cfg.github.required_labels, ["powerqueue"]);
    assert!(!cfg.github.enabled);
    assert_eq!(cfg.github.execution_project, "default");
}
