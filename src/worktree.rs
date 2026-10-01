//! Git worktree management.
//!
//! All operations shell out to `git`; we do not link libgit2 to keep the
//! binary small and to behave exactly like the user's git (hooks, config,
//! credential helpers).

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// One entry from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub prunable: bool,
}

/// Operations on one repository.
#[derive(Debug, Clone)]
pub struct Repo {
    pub path: PathBuf,
    pub git_binary: String,
}

impl Repo {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), git_binary: "git".to_string() }
    }

    /// Run `git <args>` in `cwd` (defaults to the repo), returning stdout.
    pub fn git(&self, cwd: Option<&Path>, args: &[&str]) -> Result<String> {
        let _ = (cwd, args);
        todo!("TODO(agent-runtime): run git, include stderr in error")
    }

    pub fn is_repo(&self) -> bool {
        todo!("TODO(agent-runtime): rev-parse --is-inside-work-tree")
    }

    /// `origin/HEAD` target, else `main`/`master` if they exist, else current branch.
    pub fn default_branch(&self) -> Result<String> {
        todo!("TODO(agent-runtime)")
    }

    pub fn fetch(&self) -> Result<()> {
        todo!("TODO(agent-runtime): git fetch --prune origin (ignore if no remote)")
    }

    pub fn has_remote(&self) -> Result<bool> {
        todo!("TODO(agent-runtime)")
    }

    pub fn branch_exists(&self, branch: &str) -> Result<bool> {
        let _ = branch;
        todo!("TODO(agent-runtime)")
    }

    pub fn list_worktrees(&self) -> Result<Vec<WorktreeEntry>> {
        todo!("TODO(agent-runtime): parse --porcelain")
    }

    /// Create `path` as a worktree on `branch`, creating the branch from
    /// `base` if it does not exist. Idempotent: an existing worktree on the
    /// same branch is reused.
    pub fn add_worktree(&self, path: &Path, branch: &str, base: &str) -> Result<()> {
        let _ = (path, branch, base);
        todo!("TODO(agent-runtime)")
    }

    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        let _ = (path, force);
        todo!("TODO(agent-runtime): git worktree remove [--force]; then prune")
    }

    pub fn prune_worktrees(&self) -> Result<()> {
        todo!("TODO(agent-runtime)")
    }

    pub fn delete_branch(&self, branch: &str, force: bool) -> Result<()> {
        let _ = (branch, force);
        todo!("TODO(agent-runtime)")
    }

    /// `git status --porcelain` non-empty.
    pub fn is_dirty(&self, worktree: &Path) -> Result<bool> {
        let _ = worktree;
        todo!("TODO(agent-runtime)")
    }

    /// Commits on `branch` not on `origin/<branch>` (or not on `base` when no remote).
    pub fn unpushed_commits(&self, worktree: &Path, branch: &str, base: &str) -> Result<u32> {
        let _ = (worktree, branch, base);
        todo!("TODO(agent-runtime)")
    }

    pub fn push_branch(&self, worktree: &Path, branch: &str) -> Result<()> {
        let _ = (worktree, branch);
        todo!("TODO(agent-runtime): push -u origin branch")
    }

    /// Short log of the branch relative to base, for completion summaries.
    pub fn log_since(&self, worktree: &Path, base: &str, max: u32) -> Result<Vec<String>> {
        let _ = (worktree, base, max);
        todo!("TODO(agent-runtime)")
    }

    pub fn head_sha(&self, worktree: &Path) -> Result<String> {
        let _ = worktree;
        todo!("TODO(agent-runtime)")
    }
}

/// Render `template` (`pq/{key}`) with the task key slug and short id.
pub fn branch_name(template: &str, key_slug: &str, short_id: &str) -> String {
    template.replace("{key}", key_slug).replace("{id}", short_id)
}

/// Run each `sh -c` command in `cwd`, stopping at the first failure.
/// Returns combined output of all commands.
pub fn run_commands(cwd: &Path, commands: &[String], env: &[(String, String)]) -> Result<String> {
    let _ = (cwd, commands, env);
    todo!("TODO(agent-runtime)")
}
