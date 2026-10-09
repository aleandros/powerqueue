//! Exercises `powerqueue::worktree::Repo` against real temporary git repositories.
//! Skipped when `git` is not on PATH.

use std::path::{Path, PathBuf};
use std::process::Command;

use powerqueue::worktree::{BasePreference, Repo, run_commands};
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

fn rev(cwd: &Path, rev: &str) -> String {
    let out = Command::new("git").args(["rev-parse", rev]).current_dir(cwd).output().expect("git runs");
    assert!(out.status.success(), "git rev-parse {rev}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `repo` gets a bare `origin`; returns a second clone that plays "GitHub
/// merged a PR": commits pushed from it land on `origin/main` only.
fn with_remote(dir: &TempDir, repo: &Repo) -> PathBuf {
    let bare = dir.path().join("origin.git");
    let out = Command::new("git").args(["init", "-q", "--bare", "-b", "main"]).arg(&bare).output().unwrap();
    assert!(out.status.success());
    git(&repo.path, &["remote", "add", "origin", &bare.to_string_lossy()]);
    git(&repo.path, &["push", "-q", "-u", "origin", "main"]);
    let other = dir.path().join("other");
    let out = Command::new("git").args(["clone", "-q"]).arg(&bare).arg(&other).output().unwrap();
    assert!(out.status.success());
    other
}

#[test]
fn new_branch_starts_from_fetched_origin_not_stale_local_base() {
    let Some((dir, repo)) = fixture() else { return };
    let other = with_remote(&dir, &repo);
    commit_file(&other, "merged.txt", "blocker merged");
    git(&other, &["push", "-q", "origin", "main"]);
    let merged = rev(&other, "HEAD");

    repo.fetch().unwrap();
    assert_ne!(rev(&repo.path, "main"), merged, "fetch alone leaves the local main behind");
    let start = repo.start_point("main", BasePreference::Remote).unwrap();
    assert_eq!((start.reference.as_str(), start.sha.as_str()), ("origin/main", merged.as_str()));
    // A failed fetch still uses the last fetched origin/main: it is newer.
    assert_eq!(repo.start_point("main", BasePreference::Newest).unwrap(), start);
    // With fetch_before_start off the local branch is used.
    let local = repo.start_point("main", BasePreference::Local).unwrap();
    assert_eq!((local.reference.as_str(), local.sha), ("main", rev(&repo.path, "main")));
    // Commits merged upstream are not "unpushed" work of a lagging-base branch.
    let wt0 = dir.path().join("wt").join("zero");
    repo.add_worktree_on(&wt0, "pq/zero", Some(&start.sha)).unwrap();
    assert_eq!(repo.unpushed_commits(&wt0, "pq/zero", "main").unwrap(), 0);
    commit_file(&wt0, "own.txt", "own work");
    assert_eq!(repo.unpushed_commits(&wt0, "pq/zero", "main").unwrap(), 1);

    let wt = dir.path().join("wt").join("next");
    repo.add_worktree(&wt, "pq/next", &start.sha).unwrap();
    assert!(wt.join("merged.txt").exists(), "the next task's branch holds the merged commit");
    assert_eq!(rev(&wt, "HEAD"), merged);
    // The new branch does not track the base: a bare push/pull can't hit main.
    let upstream = Command::new("git").args(["config", "branch.pq/next.merge"]).current_dir(&repo.path).output().unwrap();
    assert!(!upstream.status.success(), "pq/next must not track origin/main");
}

#[test]
fn start_point_without_remote_branch_uses_local_base() {
    let Some((_dir, repo)) = fixture() else { return };
    let start = repo.start_point("main", BasePreference::Remote).unwrap();
    assert_eq!(start.reference, "main");
    assert!(repo.start_point("nope", BasePreference::Remote).is_err());
}

#[test]
fn newest_start_point_keeps_local_commits_the_remote_lacks() {
    let Some((dir, repo)) = fixture() else { return };
    let _other = with_remote(&dir, &repo);
    commit_file(&repo.path, "local.txt", "local only");
    let start = repo.start_point("main", BasePreference::Newest).unwrap();
    assert_eq!((start.reference.as_str(), start.sha), ("main", rev(&repo.path, "main")));
    // A fresh fetch trusts the remote.
    assert_eq!(repo.start_point("main", BasePreference::Remote).unwrap().reference, "origin/main");
}

#[test]
fn fast_forwards_clean_checkout_and_skips_unsafe_cases() {
    use powerqueue::worktree::FastForward;
    let Some((dir, repo)) = fixture() else { return };
    let other = with_remote(&dir, &repo);
    assert_eq!(repo.fast_forward_branch("main").unwrap(), FastForward::UpToDate);
    // The repository's hooks never run in the user's checkout.
    let hook = repo.path.join(".git/hooks/post-merge");
    std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    commit_file(&other, "one.txt", "one");
    git(&other, &["push", "-q", "origin", "main"]);
    repo.fetch().unwrap();
    // Uncommitted change to a tracked file in the main checkout: left alone.
    std::fs::write(repo.path.join("README.md"), "edited\n").unwrap();
    assert!(matches!(repo.fast_forward_branch("main").unwrap(), FastForward::Skipped(why) if why.contains("uncommitted")));
    git(&repo.path, &["checkout", "-q", "--", "README.md"]);
    // Untracked files do not block it.
    std::fs::write(repo.path.join("scratch.txt"), "x\n").unwrap();
    let before = rev(&repo.path, "main");
    assert_eq!(
        repo.fast_forward_branch("main").unwrap(),
        FastForward::Updated { from: before, to: rev(&repo.path, "origin/main") }
    );
    assert!(repo.path.join("one.txt").exists(), "the checkout's files moved too");
    assert!(!repo.path.join("hook-ran").exists(), "post-merge hook must not run");

    // Not checked out anywhere: the ref moves.
    git(&repo.path, &["checkout", "-q", "-b", "elsewhere"]);
    commit_file(&other, "two.txt", "two");
    git(&other, &["push", "-q", "origin", "main"]);
    repo.fetch().unwrap();
    assert!(matches!(repo.fast_forward_branch("main").unwrap(), FastForward::Updated { .. }));
    assert_eq!(rev(&repo.path, "main"), rev(&repo.path, "origin/main"));

    // Local commits the remote lacks: never rewritten.
    git(&repo.path, &["checkout", "-q", "main"]);
    commit_file(&repo.path, "local.txt", "local only");
    commit_file(&other, "three.txt", "three");
    git(&other, &["push", "-q", "origin", "main"]);
    repo.fetch().unwrap();
    let local = rev(&repo.path, "main");
    assert!(matches!(repo.fast_forward_branch("main").unwrap(), FastForward::Skipped(why) if why.contains("lacks")));
    assert_eq!(rev(&repo.path, "main"), local);
    assert!(matches!(repo.fast_forward_branch("missing").unwrap(), FastForward::Skipped(_)));
}

#[test]
fn fast_forward_skips_a_rebase_in_progress() {
    use powerqueue::worktree::FastForward;
    let Some((dir, repo)) = fixture() else { return };
    let other = with_remote(&dir, &repo);
    commit_file(&other, "one.txt", "one");
    git(&other, &["push", "-q", "origin", "main"]);
    repo.fetch().unwrap();
    // What `git rebase` leaves: main detached, its name in rebase-merge/head-name.
    git(&repo.path, &["checkout", "-q", "--detach", "main"]);
    std::fs::create_dir_all(repo.path.join(".git/rebase-merge")).unwrap();
    std::fs::write(repo.path.join(".git/rebase-merge/head-name"), "refs/heads/main\n").unwrap();
    let before = rev(&repo.path, "main");
    assert!(matches!(repo.fast_forward_branch("main").unwrap(), FastForward::Skipped(why) if why.contains("rebase")));
    assert_eq!(rev(&repo.path, "main"), before);
}

#[test]
fn review_round_branch_catches_up_with_commits_pushed_to_the_pr() {
    use powerqueue::worktree::FastForward;
    let Some((dir, repo)) = fixture() else { return };
    let other = with_remote(&dir, &repo);
    // The task's branch was pushed and its worktree removed at the hand-off.
    let wt = dir.path().join("wt").join("review");
    repo.add_worktree(&wt, "pq/review", "main").unwrap();
    commit_file(&wt, "own.txt", "own work");
    git(&wt, &["push", "-q", "origin", "pq/review"]);
    repo.remove_worktree(&wt, true).unwrap();
    // A reviewer commits a suggestion on GitHub.
    git(&other, &["fetch", "-q", "origin", "pq/review"]);
    git(&other, &["checkout", "-q", "-b", "pq/review", "FETCH_HEAD"]);
    commit_file(&other, "suggestion.txt", "reviewer's suggestion");
    git(&other, &["push", "-q", "origin", "pq/review"]);
    let pushed = rev(&other, "HEAD");

    // The review round: fetch, fast-forward the kept branch, recreate the worktree.
    repo.fetch().unwrap();
    assert!(matches!(repo.fast_forward_branch("pq/review").unwrap(), FastForward::Updated { .. }));
    repo.add_worktree(&wt, "pq/review", "main").unwrap();
    assert_eq!(rev(&wt, "HEAD"), pushed);
    assert!(wt.join("suggestion.txt").exists(), "the session sees the reviewer's commit");
}

#[test]
fn show_file_blob_ids_and_current_branch() {
    let Some((_dir, repo)) = fixture() else { return };
    assert_eq!(repo.current_branch().unwrap().as_deref(), Some("main"));
    assert_eq!(repo.show_file("main", "README.md").unwrap().as_deref(), Some("hello\n"));
    assert_eq!(repo.show_file("main", "missing.txt").unwrap(), None);
    assert_eq!(repo.show_file("main", "no/such/dir/PRIORITY.md").unwrap(), None, "a missing directory is just absent");
    std::fs::write(repo.path.join("README.md"), "dirty\n").unwrap();
    assert_eq!(repo.show_file("main", "README.md").unwrap().as_deref(), Some("hello\n"), "working tree ignored");
    let err = repo.show_file("nope", "README.md").unwrap_err().to_string();
    assert!(err.contains("does not resolve"), "{err}");

    let before = repo.blob_ids("main", &["README.md", "missing.txt"]).unwrap();
    assert_eq!(before.len(), 1);
    assert!(before.contains_key("README.md"));
    commit_file(&repo.path, "README.md", "change");
    let after = repo.blob_ids("main", &["README.md"]).unwrap();
    assert_ne!(before["README.md"], after["README.md"]);
    assert!(repo.blob_ids("nope", &["README.md"]).is_err());
    // Non-ASCII paths come back verbatim (git would quote them otherwise).
    std::fs::create_dir_all(repo.path.join("docs")).unwrap();
    commit_file(&repo.path, "docs/règles.md", "accents");
    let ids = repo.blob_ids("main", &["docs/règles.md"]).unwrap();
    assert!(ids.contains_key("docs/règles.md"), "{ids:?}");
    assert_eq!(repo.show_file("main", "docs/règles.md").unwrap().as_deref(), Some("docs/règles.md\n"));

    git(&repo.path, &["checkout", "-q", "--detach"]);
    assert_eq!(repo.current_branch().unwrap(), None);
}
