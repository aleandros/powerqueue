//! `powerqueue reset` against an isolated `POWERQUEUE_HOME`.
//!
//! The config points tmux at a private socket that no server listens on, so
//! the command never sees (let alone kills) the user's own `powerqueue`
//! session. Worktree tests need `git` and skip themselves without it.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;

const GIT_ID: [&str; 4] = ["-c", "user.name=powerqueue-test", "-c", "user.email=test@example.invalid"];

fn pq(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("powerqueue").expect("binary builds");
    cmd.env("POWERQUEUE_HOME", home)
        .env("POWERQUEUE_SECRETS", "file")
        .env_remove("NO_COLOR")
        .env_remove("LINEAR_API_KEY")
        .env_remove("JEV_API_KEY");
    cmd
}

/// Minimal config: the repo, an explicit worktree root and a tmux socket nobody uses.
fn write_config(home: &Path, repo: &Path, worktree_root: &Path) {
    let config_dir = home.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let socket = format!("powerqueue-test-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[repo]\npath = \"{}\"\nworktree_root = \"{}\"\n\n[tmux]\nsocket_name = \"{socket}\"\n",
            repo.display(),
            worktree_root.display()
        ),
    )
    .unwrap();
}

fn git(cwd: &Path, args: &[&str]) {
    let out = StdCommand::new("git").args(GIT_ID).args(args).current_dir(cwd).output().expect("git runs");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn git_stdout(cwd: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").args(args).current_dir(cwd).output().expect("git runs");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A fresh repo on `main` with one commit, or `None` when git is missing.
fn git_repo(dir: &Path) -> Option<PathBuf> {
    if which::which("git").is_err() {
        eprintln!("skipping: git not on PATH");
        return None;
    }
    let root = dir.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    Some(root)
}

fn task_dirs(home: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(home.join("state").join("tasks")).map(|rd| rd.map(|e| e.unwrap().path()).collect()).unwrap_or_default()
}

#[test]
fn dry_run_lists_tasks_and_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_config(home.path(), repo.path(), &home.path().join("wt"));
    pq(home.path()).args(["add", "First", "--key", "R-1"]).assert().success();
    pq(home.path()).args(["add", "Second", "--key", "R-2", "--paused"]).assert().success();
    // A per-task state dir, as the launcher would leave behind.
    std::fs::create_dir_all(home.path().join("state").join("tasks").join("some-task")).unwrap();

    let out = pq(home.path()).args(["--json", "reset", "--dry-run"]).assert().success().get_output().stdout.clone();
    let plan: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let keys: Vec<&str> = plan["tasks"].as_array().unwrap().iter().map(|t| t["key"].as_str().unwrap()).collect();
    assert_eq!(keys, vec!["R-1", "R-2"]);
    assert_eq!(plan["tasks_by_state"]["queued"], 1);
    assert_eq!(plan["tasks_by_state"]["paused"], 1);
    assert_eq!(plan["database"]["counts"]["tasks"], 2);
    assert_eq!(plan["database"]["clear_kv"], false);
    assert_eq!(plan["daemon"]["alive"], false);
    assert_eq!(plan["tmux"]["session_exists"], false);
    assert_eq!(plan["task_dirs"].as_array().unwrap().len(), 1);
    assert_eq!(plan["linear"]["requested"], false);

    // Human output names the tasks too, and nothing was touched.
    pq(home.path())
        .args(["reset", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("R-1"))
        .stdout(predicate::str::contains("R-2"))
        .stdout(predicate::str::contains("Dry run: nothing was changed"));
    pq(home.path())
        .args(["--json", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"key\": \"R-1\""))
        .stdout(predicate::str::contains("\"key\": \"R-2\""));
    assert_eq!(task_dirs(home.path()).len(), 1);
}

#[test]
fn reset_requires_yes_without_a_tty_and_then_empties_everything() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_config(home.path(), repo.path(), &home.path().join("wt"));
    pq(home.path()).args(["add", "First", "--key", "R-1"]).assert().success();
    std::fs::create_dir_all(home.path().join("state").join("tasks").join("some-task")).unwrap();
    std::fs::write(home.path().join("state").join("tasks").join("stray-file"), "x").unwrap();

    // stdin is a pipe here, so the prompt cannot be shown.
    pq(home.path()).arg("reset").assert().code(1).stderr(predicate::str::contains("--yes"));
    pq(home.path()).args(["--json", "status"]).assert().success().stdout(predicate::str::contains("\"key\": \"R-1\""));

    pq(home.path())
        .args(["reset", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("powerqueue reset done"))
        .stdout(predicate::str::contains("task dirs  removed 2"))
        .stdout(predicate::str::contains("Kept: config.toml, secrets, PRIORITY.md and logs"))
        .stdout(predicate::str::contains("Linear issues were left as-is"));

    pq(home.path()).arg("status").assert().success().stdout(predicate::str::contains("no open tasks"));
    pq(home.path()).args(["status", "--all"]).assert().success().stdout(predicate::str::contains("R-1").not());
    assert!(task_dirs(home.path()).is_empty(), "{:?}", task_dirs(home.path()));
    assert!(home.path().join("config").join("config.toml").exists(), "config is kept");

    // Idempotent, and the JSON result reports the (now empty) counts.
    let out = pq(home.path()).args(["--json", "reset", "--yes"]).assert().success().get_output().stdout.clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["result"]["database"]["tasks"], 0);
    assert_eq!(v["result"]["errors"].as_array().unwrap().len(), 0);
}

#[test]
fn everything_clears_kv_but_default_keeps_it() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_config(home.path(), repo.path(), &home.path().join("wt"));
    pq(home.path()).args(["add", "First", "--key", "R-1"]).assert().success();
    pq(home.path()).args(["budget", "set-observed", "40%"]).assert().success();

    let out = pq(home.path()).args(["--json", "reset", "--yes"]).assert().success().get_output().stdout.clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["result"]["database"]["tasks"], 1);
    assert_eq!(v["result"]["database"]["kv"], 0, "kv is kept by default");
    assert!(v["plan"]["database"]["counts"]["kv"].as_u64().unwrap() >= 1);

    let out = pq(home.path()).args(["--json", "reset", "--yes", "--everything"]).assert().success().get_output().stdout.clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(v["result"]["database"]["kv"].as_u64().unwrap() >= 1, "{v}");
    assert_eq!(v["plan"]["database"]["clear_kv"], true);
}

#[test]
fn dirty_worktrees_survive_unless_forced() {
    let home = tempfile::tempdir().unwrap();
    let Some(repo) = git_repo(home.path()) else { return };
    let root = home.path().join("wt");
    write_config(home.path(), &repo, &root);
    pq(home.path()).args(["add", "Worktree task", "--key", "ENG-1"]).assert().success();

    // Two real worktrees under the configured root: one clean, one dirty; plus a leftover directory.
    let clean = root.join("eng-2");
    let dirty = root.join("eng-1");
    std::fs::create_dir_all(&root).unwrap();
    git(&repo, &["worktree", "add", "-q", "-b", "pq/eng-2", clean.to_str().unwrap(), "main"]);
    git(&repo, &["worktree", "add", "-q", "-b", "pq/eng-1", dirty.to_str().unwrap(), "main"]);
    std::fs::write(dirty.join("wip.txt"), "unsaved work\n").unwrap();
    let leftover = root.join("old-task");
    std::fs::create_dir_all(leftover.join("src")).unwrap();
    std::fs::write(leftover.join("src").join("x.rs"), "").unwrap();

    let out = pq(home.path()).args(["--json", "reset", "--dry-run"]).assert().success().get_output().stdout.clone();
    let plan: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let wts = plan["worktrees"].as_array().unwrap();
    assert_eq!(wts.len(), 3, "{wts:#?}");
    let by_name = |name: &str| wts.iter().find(|w| w["path"].as_str().unwrap().ends_with(name)).unwrap();
    assert_eq!(by_name("eng-1")["action"], "keep");
    assert_eq!(by_name("eng-1")["dirty"], true);
    assert_eq!(by_name("eng-2")["action"], "remove");
    assert_eq!(by_name("old-task")["action"], "remove");
    assert_eq!(by_name("old-task")["origin"], "leftover");

    pq(home.path())
        .args(["reset", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("worktrees  removed 2, kept 1"))
        .stdout(predicate::str::contains("eng-1"))
        .stdout(predicate::str::contains("uncommitted changes"));
    assert!(dirty.join("wip.txt").exists(), "dirty worktree survives");
    assert!(!clean.exists(), "clean worktree removed");
    assert!(!leftover.exists(), "leftover directory removed");
    assert!(repo.join("README.md").exists(), "the repository itself is untouched");
    let list = git_stdout(&repo, &["worktree", "list", "--porcelain"]);
    assert!(list.contains("eng-1") && !list.contains("eng-2"), "{list}");
    assert!(git_stdout(&repo, &["branch", "--list", "pq/*"]).contains("pq/eng-2"), "branches are kept by default");

    // --force removes it, and --delete-branches drops the branches still recorded on a task or
    // worktree (pq/eng-2 is an orphan by now: its task and worktree went in the first run) but never main.
    pq(home.path())
        .args(["reset", "--yes", "--force", "--delete-branches"])
        .assert()
        .success()
        .stdout(predicate::str::contains("worktrees  removed 1, kept 0"))
        .stdout(predicate::str::contains("branches   deleted 1"));
    assert!(!dirty.exists());
    let branches = git_stdout(&repo, &["branch", "--list"]);
    assert!(!branches.contains("pq/eng-1"), "{branches}");
    assert!(branches.contains("pq/eng-2"), "orphan branches are not guessed at: {branches}");
    assert!(branches.contains("main"), "{branches}");
    assert!(!git_stdout(&repo, &["worktree", "list", "--porcelain"]).contains("eng-1"));
}

#[test]
fn never_deletes_outside_the_worktree_root() {
    let home = tempfile::tempdir().unwrap();
    let Some(repo) = git_repo(home.path()) else { return };
    let root = home.path().join("wt");
    write_config(home.path(), &repo, &root);
    // A worktree the repo knows about but which lives elsewhere.
    let elsewhere = home.path().join("elsewhere").join("eng-9");
    git(&repo, &["worktree", "add", "-q", "-b", "pq/eng-9", elsewhere.to_str().unwrap(), "main"]);
    // A symlink inside the root that points at it.
    std::fs::create_dir_all(&root).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&elsewhere, root.join("link")).unwrap();

    pq(home.path()).args(["reset", "--yes", "--force"]).assert().success();
    assert!(elsewhere.join("README.md").exists(), "worktree outside the root survives");
    assert!(repo.join("README.md").exists());
    #[cfg(unix)]
    assert!(std::fs::symlink_metadata(root.join("link")).is_ok(), "the symlink is kept, not followed");
}
