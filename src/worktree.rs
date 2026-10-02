//! Git worktree management.
//!
//! All operations shell out to `git`; we do not link libgit2 to keep the
//! binary small and to behave exactly like the user's git (hooks, config,
//! credential helpers). Every error names the command line and carries
//! git's stderr.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
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
    /// Fails with the command line and git's stderr on a non-zero exit.
    pub fn git(&self, cwd: Option<&Path>, args: &[&str]) -> Result<String> {
        let out = self.git_output(cwd, args)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!(
                "`{}` failed ({}) in {}: {}",
                self.describe(args),
                out.status,
                cwd.unwrap_or(&self.path).display(),
                if stderr.is_empty() { "<no output>" } else { &stderr }
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Run `git <args>` and return the raw output without interpreting the status.
    fn git_output(&self, cwd: Option<&Path>, args: &[&str]) -> Result<std::process::Output> {
        let cwd = cwd.unwrap_or(&self.path);
        Command::new(&self.git_binary)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            // Never block on a credential prompt from a daemon.
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .with_context(|| format!("cannot run `{}` in {} (is git installed?)", self.describe(args), cwd.display()))
    }

    /// Exit status only (for `--quiet` style probes).
    fn git_succeeds(&self, cwd: Option<&Path>, args: &[&str]) -> Result<bool> {
        Ok(self.git_output(cwd, args)?.status.success())
    }

    fn describe(&self, args: &[&str]) -> String {
        let mut parts = vec![self.git_binary.clone()];
        parts.extend(args.iter().map(|a| crate::tmux::shell_quote(a)));
        parts.join(" ")
    }

    /// True when `path` is inside a git work tree (false if git is missing).
    pub fn is_repo(&self) -> bool {
        self.path.is_dir()
            && self
                .git_output(None, &["rev-parse", "--is-inside-work-tree"])
                .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
                .unwrap_or(false)
    }

    /// `origin/HEAD` target, else `main`/`master` if they exist, else current branch.
    pub fn default_branch(&self) -> Result<String> {
        if let Ok(out) = self.git_output(None, &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
            && out.status.success()
        {
            let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Some(name) = full.strip_prefix("refs/remotes/origin/")
                && !name.is_empty()
            {
                return Ok(name.to_string());
            }
        }
        for candidate in ["main", "master"] {
            if self.branch_exists(candidate)? {
                return Ok(candidate.to_string());
            }
        }
        let current = self.git(None, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_string();
        if current.is_empty() || current == "HEAD" {
            bail!("cannot determine the default branch of {} (no origin/HEAD, main or master)", self.path.display());
        }
        Ok(current)
    }

    /// `git fetch --prune origin`; a no-op when the repo has no `origin`.
    pub fn fetch(&self) -> Result<()> {
        if !self.has_remote()? {
            tracing::debug!(repo = %self.path.display(), "no origin remote; skipping fetch");
            return Ok(());
        }
        self.git(None, &["fetch", "--prune", "origin"])?;
        tracing::debug!(repo = %self.path.display(), "fetched origin");
        Ok(())
    }

    /// Whether a remote named `origin` is configured.
    pub fn has_remote(&self) -> Result<bool> {
        let out = self.git(None, &["remote"])?;
        Ok(out.lines().any(|l| l.trim() == "origin"))
    }

    /// Whether a local branch exists.
    pub fn branch_exists(&self, branch: &str) -> Result<bool> {
        let r = format!("refs/heads/{branch}");
        self.git_succeeds(None, &["show-ref", "--verify", "--quiet", &r])
    }

    /// Whether `origin/<branch>` exists locally (after a fetch).
    pub fn remote_branch_exists(&self, branch: &str) -> Result<bool> {
        let r = format!("refs/remotes/origin/{branch}");
        self.git_succeeds(None, &["show-ref", "--verify", "--quiet", &r])
    }

    /// Parse `git worktree list --porcelain`.
    pub fn list_worktrees(&self) -> Result<Vec<WorktreeEntry>> {
        let text = self.git(None, &["worktree", "list", "--porcelain"])?;
        Ok(parse_worktree_list(&text))
    }

    /// Create `path` as a worktree on `branch`, creating the branch from
    /// `base` if it does not exist. Idempotent: an existing worktree on the
    /// same branch is reused; one on a different branch is an error.
    pub fn add_worktree(&self, path: &Path, branch: &str, base: &str) -> Result<()> {
        let wanted = normalize(path);
        if let Some(existing) = self.list_worktrees()?.into_iter().find(|w| normalize(&w.path) == wanted) {
            match existing.branch.as_deref() {
                Some(b) if b == branch => {
                    tracing::debug!(path = %path.display(), branch, "worktree already exists");
                    return Ok(());
                }
                other => bail!(
                    "{} is already a worktree on {} (wanted branch {branch}); remove it or pick another path",
                    path.display(),
                    other.map(|b| format!("branch `{b}`")).unwrap_or_else(|| "a detached HEAD".to_string())
                ),
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let path_s = path.to_string_lossy();
        if self.branch_exists(branch)? {
            self.git(None, &["worktree", "add", &path_s, branch])?;
        } else {
            self.git(None, &["worktree", "add", "-b", branch, &path_s, base])?;
        }
        tracing::info!(path = %path.display(), branch, base, "created worktree");
        Ok(())
    }

    /// `git worktree remove [--force] <path>` followed by a prune. Removing a
    /// path that is no longer registered is not an error.
    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        let wanted = normalize(path);
        let registered = self.list_worktrees()?.iter().any(|w| normalize(&w.path) == wanted);
        if registered {
            let path_s = path.to_string_lossy();
            let mut args = vec!["worktree", "remove"];
            if force {
                args.push("--force");
            }
            args.push(&path_s);
            self.git(None, &args)?;
            tracing::info!(path = %path.display(), force, "removed worktree");
        } else {
            tracing::debug!(path = %path.display(), "worktree not registered; nothing to remove");
        }
        self.prune_worktrees()
    }

    /// `git worktree prune`.
    pub fn prune_worktrees(&self) -> Result<()> {
        self.git(None, &["worktree", "prune"])?;
        Ok(())
    }

    /// Delete a local branch (`-d`, or `-D` with `force`).
    pub fn delete_branch(&self, branch: &str, force: bool) -> Result<()> {
        let flag = if force { "-D" } else { "-d" };
        self.git(None, &["branch", flag, branch])?;
        tracing::info!(branch, force, "deleted branch");
        Ok(())
    }

    /// `git status --porcelain` non-empty.
    pub fn is_dirty(&self, worktree: &Path) -> Result<bool> {
        let out = self.git(Some(worktree), &["status", "--porcelain", "--untracked-files=normal"])?;
        Ok(out.lines().any(|l| !l.trim().is_empty()))
    }

    /// Commits on `branch` not on `origin/<branch>` (or not on `base` when
    /// the remote branch does not exist).
    pub fn unpushed_commits(&self, worktree: &Path, branch: &str, base: &str) -> Result<u32> {
        let range =
            if self.remote_branch_exists(branch)? { format!("origin/{branch}..{branch}") } else { format!("{base}..{branch}") };
        let out = self.git(Some(worktree), &["rev-list", "--count", &range])?;
        out.trim().parse::<u32>().with_context(|| format!("unexpected `git rev-list --count {range}` output `{}`", out.trim()))
    }

    /// `git push -u origin <branch>`.
    pub fn push_branch(&self, worktree: &Path, branch: &str) -> Result<()> {
        if !self.has_remote()? {
            bail!("cannot push {branch}: repository {} has no `origin` remote", self.path.display());
        }
        self.git(Some(worktree), &["push", "-u", "origin", branch])?;
        tracing::info!(branch, "pushed branch");
        Ok(())
    }

    /// Short log of the branch relative to base, for completion summaries.
    pub fn log_since(&self, worktree: &Path, base: &str, max: u32) -> Result<Vec<String>> {
        let range = format!("{base}..HEAD");
        let n = format!("--max-count={max}");
        let out = self.git(Some(worktree), &["log", "--oneline", "--no-decorate", &n, &range])?;
        Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
    }

    /// Full SHA of `HEAD` in `worktree`.
    pub fn head_sha(&self, worktree: &Path) -> Result<String> {
        Ok(self.git(Some(worktree), &["rev-parse", "HEAD"])?.trim().to_string())
    }
}

/// Resolve symlinks where possible so `/tmp/x` and `/private/tmp/x` compare equal.
fn normalize(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Parse the output of `git worktree list --porcelain`: blocks separated by
/// blank lines, each starting with `worktree <path>`.
pub fn parse_worktree_list(text: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            current = Some(WorktreeEntry {
                path: PathBuf::from(path),
                head: String::new(),
                branch: None,
                bare: false,
                detached: false,
                prunable: false,
            });
            continue;
        }
        let Some(e) = current.as_mut() else { continue };
        if let Some(sha) = line.strip_prefix("HEAD ") {
            e.head = sha.to_string();
        } else if let Some(r) = line.strip_prefix("branch ") {
            e.branch = Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string());
        } else if line == "bare" {
            e.bare = true;
        } else if line == "detached" {
            e.detached = true;
        } else if line.starts_with("prunable") {
            e.prunable = true;
        }
    }
    if let Some(e) = current {
        entries.push(e);
    }
    entries
}

/// Render `template` (`pq/{key}`) with the task key slug and short id.
pub fn branch_name(template: &str, key_slug: &str, short_id: &str) -> String {
    template.replace("{key}", key_slug).replace("{id}", short_id)
}

/// Run each `sh -c` command in `cwd`, stopping at the first failure.
/// Returns combined output of all commands; a failure's error carries the
/// output collected so far, including the failing command's.
pub fn run_commands(cwd: &Path, commands: &[String], env: &[(String, String)]) -> Result<String> {
    let mut combined = String::new();
    for cmd in commands {
        tracing::debug!(cwd = %cwd.display(), command = %cmd, "running command");
        let out = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .envs(env.iter().map(|(k, v)| (k, v)))
            .output()
            .with_context(|| format!("cannot run `sh -c {}` in {}", crate::tmux::shell_quote(cmd), cwd.display()))?;
        combined.push_str(&format!("$ {cmd}\n"));
        combined.push_str(&String::from_utf8_lossy(&out.stdout));
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
        if !out.status.success() {
            bail!("command `{cmd}` failed ({}) in {}:\n{}", out.status, cwd.display(), combined.trim_end());
        }
    }
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_list() {
        let text = "worktree /repo\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\n\
                    worktree /repo/.wt/x\nHEAD 2222222222222222222222222222222222222222\nbranch refs/heads/pq/x\n\n\
                    worktree /repo/.wt/y\nHEAD 3333333333333333333333333333333333333333\ndetached\nprunable gitdir file points to non-existent location\n\n\
                    worktree /bare.git\nbare\n";
        let list = parse_worktree_list(text);
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].path, PathBuf::from("/repo"));
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert!(list[0].head.starts_with("1111"));
        assert_eq!(list[1].branch.as_deref(), Some("pq/x"));
        assert!(list[2].detached);
        assert!(list[2].prunable);
        assert_eq!(list[2].branch, None);
        assert!(list[3].bare);
        assert!(parse_worktree_list("").is_empty());
    }

    #[test]
    fn branch_template() {
        assert_eq!(branch_name("pq/{key}", "eng-1", "abcd1234"), "pq/eng-1");
        assert_eq!(branch_name("agent/{id}-{key}", "eng-1", "abcd1234"), "agent/abcd1234-eng-1");
    }
}
