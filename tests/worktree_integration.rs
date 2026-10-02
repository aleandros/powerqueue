//! Exercises `powerqueue::worktree::Repo` against real temporary git repositories.
//! Skipped when `git` is not on PATH.

use std::path::{Path, PathBuf};
use std::process::Command;

use powerqueue::worktree::{Repo, run_commands};
use tempfile::TempDir;

const GIT_ID: [&str; 4] = ["-c", "user.name=powerqueue-test", "-c", "user.email=test@example.invalid"];

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(GIT_ID).args(args).current_dir(cwd).output().expect("git runs");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// A fresh repo on `main` with one committed file.
fn fixture() -> Option<(TempDir, Repo)> {
    if which::which("git").is_err() {
        eprintln!("skipping: git not on PATH");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    let repo = Repo::new(root);
    Some((dir, repo))
}

fn commit_file(wt: &Path, name: &str, msg: &str) {
    std::fs::write(wt.join(name), format!("{name}\n")).unwrap();
    git(wt, &["add", "."]);
    git(wt, &["commit", "-q", "-m", msg]);
}

#[test]
fn detects_repo_and_default_branch() {
    let Some((dir, repo)) = fixture() else { return };
    assert!(repo.is_repo());
    assert!(!Repo::new(dir.path().join("nope")).is_repo());
    assert_eq!(repo.default_branch().unwrap(), "main");
    assert!(!repo.has_remote().unwrap());
    // No remote: fetch is a silent no-op, pushing is a clear error.
    repo.fetch().unwrap();
    let err = repo.push_branch(&repo.path, "main").unwrap_err().to_string();
    assert!(err.contains("no `origin` remote"), "{err}");
}

#[test]
fn default_branch_falls_back_to_current_branch() {
    let Some((_dir, repo)) = fixture() else { return };
    git(&repo.path, &["branch", "-m", "main", "trunk"]);
    assert_eq!(repo.default_branch().unwrap(), "trunk");
}

#[test]
fn add_list_and_remove_worktree() {
    let Some((dir, repo)) = fixture() else { return };
    let wt: PathBuf = dir.path().join("worktrees").join("eng-1");

    assert!(!repo.branch_exists("pq/eng-1").unwrap());
    repo.add_worktree(&wt, "pq/eng-1", "main").unwrap();
    assert!(wt.join("README.md").exists());
    assert!(repo.branch_exists("pq/eng-1").unwrap());

    let list = repo.list_worktrees().unwrap();
    assert_eq!(list.len(), 2, "{list:?}");
    let entry = list.iter().find(|w| w.branch.as_deref() == Some("pq/eng-1")).expect("worktree listed");
    assert_eq!(entry.path.canonicalize().unwrap(), wt.canonicalize().unwrap());
    assert_eq!(entry.head, repo.head_sha(&wt).unwrap());
    assert!(!entry.detached && !entry.bare && !entry.prunable);

    // Idempotent on the same branch.
    repo.add_worktree(&wt, "pq/eng-1", "main").unwrap();
    assert_eq!(repo.list_worktrees().unwrap().len(), 2);

    // Same path, different branch: refused.
    let err = repo.add_worktree(&wt, "pq/other", "main").unwrap_err().to_string();
    assert!(err.contains("pq/eng-1") && err.contains("pq/other"), "{err}");

    // An existing branch is checked out rather than re-created.
    git(&repo.path, &["branch", "pq/pre", "main"]);
    let wt2 = dir.path().join("worktrees").join("pre");
    repo.add_worktree(&wt2, "pq/pre", "main").unwrap();
    assert_eq!(repo.list_worktrees().unwrap().len(), 3);

    // Dirty detection.
    assert!(!repo.is_dirty(&wt).unwrap());
    std::fs::write(wt.join("scratch.txt"), "x").unwrap();
    assert!(repo.is_dirty(&wt).unwrap());

    // Non-forced removal of a dirty worktree fails with git's explanation.
    let err = repo.remove_worktree(&wt, false).unwrap_err().to_string();
    assert!(err.contains("worktree remove"), "{err}");
    repo.remove_worktree(&wt, true).unwrap();
    assert!(!wt.exists());
    assert_eq!(repo.list_worktrees().unwrap().len(), 2);
    // Removing again is a no-op.
    repo.remove_worktree(&wt, true).unwrap();

    repo.delete_branch("pq/eng-1", false).unwrap();
    assert!(!repo.branch_exists("pq/eng-1").unwrap());
    let err = repo.delete_branch("pq/eng-1", false).unwrap_err().to_string();
    assert!(err.contains("branch -d"), "{err}");
}

#[test]
fn counts_unpushed_commits_and_logs() {
    let Some((dir, repo)) = fixture() else { return };
    let wt = dir.path().join("wt");
    repo.add_worktree(&wt, "pq/work", "main").unwrap();
    assert_eq!(repo.unpushed_commits(&wt, "pq/work", "main").unwrap(), 0);
    assert!(repo.log_since(&wt, "main", 10).unwrap().is_empty());

    commit_file(&wt, "a.txt", "add a");
    commit_file(&wt, "b.txt", "add b");
    assert_eq!(repo.unpushed_commits(&wt, "pq/work", "main").unwrap(), 2);

    let log = repo.log_since(&wt, "main", 10).unwrap();
    assert_eq!(log.len(), 2);
    assert!(log[0].ends_with("add b"), "{log:?}");
    assert!(log[1].ends_with("add a"), "{log:?}");
    assert_eq!(repo.log_since(&wt, "main", 1).unwrap().len(), 1);

    // Delete of an unmerged branch needs force.
    repo.remove_worktree(&wt, false).unwrap();
    assert!(repo.delete_branch("pq/work", false).is_err());
    repo.delete_branch("pq/work", true).unwrap();
}

#[test]
fn unpushed_commits_use_remote_when_present() {
    let Some((dir, repo)) = fixture() else { return };
    // A bare "origin" on disk is enough for push/fetch.
    let origin = dir.path().join("origin.git");
    git(dir.path(), &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
    git(&repo.path, &["remote", "add", "origin", origin.to_str().unwrap()]);
    assert!(repo.has_remote().unwrap());
    repo.fetch().unwrap();

    let wt = dir.path().join("wt");
    repo.add_worktree(&wt, "pq/push", "main").unwrap();
    commit_file(&wt, "a.txt", "add a");
    assert_eq!(repo.unpushed_commits(&wt, "pq/push", "main").unwrap(), 1);
    repo.push_branch(&wt, "pq/push").unwrap();
    assert!(repo.remote_branch_exists("pq/push").unwrap());
    assert_eq!(repo.unpushed_commits(&wt, "pq/push", "main").unwrap(), 0);
    commit_file(&wt, "b.txt", "add b");
    assert_eq!(repo.unpushed_commits(&wt, "pq/push", "main").unwrap(), 1);
}

#[test]
fn git_errors_name_the_command() {
    let Some((_dir, repo)) = fixture() else { return };
    let err = repo.git(None, &["rev-parse", "--verify", "nope"]).unwrap_err().to_string();
    assert!(err.contains("git rev-parse --verify nope"), "{err}");
    assert!(err.contains("fatal") || err.contains("Needed a single revision"), "{err}");
}

#[test]
fn run_commands_stops_at_first_failure() {
    let dir = tempfile::tempdir().unwrap();
    let env = vec![("PQ_TEST_VAR".to_string(), "value".to_string())];
    let ok = run_commands(dir.path(), &["echo one".into(), "echo \"$PQ_TEST_VAR\"".into()], &env).unwrap();
    assert!(ok.contains("one\n") && ok.contains("value\n"), "{ok}");

    let cmds = vec!["echo before".to_string(), "echo oops >&2; exit 7".to_string(), "echo never".to_string()];
    let err = run_commands(dir.path(), &cmds, &[]).unwrap_err().to_string();
    assert!(err.contains("exit 7"), "{err}");
    assert!(err.contains("before"), "{err}");
    assert!(err.contains("oops"), "{err}");
    assert!(!err.contains("never"), "{err}");
    assert_eq!(run_commands(dir.path(), &[], &[]).unwrap(), "");
}
