//! `powerqueue reset` — wipe queue state so the user can start over.
//!
//! The command first builds a [`ResetPlan`] without touching anything
//! (that is what `--dry-run` prints), asks for confirmation, and then runs
//! the steps in a fixed order: stop the daemon, kill the tmux session,
//! remove worktrees, delete branches (opt-in), remove per-task state
//! directories, empty the database. Every step continues past per-item
//! failures and the summary lists what was removed, kept and why.
//!
//! Config, secrets, PRIORITY.md and logs are never touched. Nothing outside
//! the worktree root, the task state directory and the database is deleted.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use dialoguer::Confirm;
use dialoguer::theme::{ColorfulTheme, SimpleTheme, Theme};
use owo_colors::{OwoColorize, Stream};
use serde::Serialize;
use tracing::{info, warn};

use crate::cli::output::print_warning;
use crate::cli::{Context, ResetArgs};
use crate::config::Config;
use crate::domain::{DaemonCommand, Task};
use crate::linear::LinearClient;
use crate::paths::Paths;
use crate::secrets::SecretKind;
use crate::store::{ResetCounts, Store};
use crate::tmux::Tmux;
use crate::worktree::Repo;

use super::status::ensure_initialised;

/// How long to wait for a running daemon to notice the shutdown request and
/// for its heartbeat to go stale.
const DAEMON_STOP_WAIT_SECS: u64 = 20;

/// Everything `reset` would do, computed without side effects.
#[derive(Debug, Clone, Serialize)]
pub struct ResetPlan {
    pub daemon: DaemonPlan,
    pub tasks: Vec<TaskPlan>,
    /// Number of tasks per state name.
    pub tasks_by_state: BTreeMap<String, usize>,
    pub tmux: TmuxPlan,
    /// Directory every worktree must live under.
    pub worktree_root: PathBuf,
    pub worktrees: Vec<WorktreePlan>,
    pub branches: Vec<BranchPlan>,
    /// Directory holding the per-task state directories.
    pub tasks_dir: PathBuf,
    pub task_dirs: Vec<PathBuf>,
    pub database: DatabasePlan,
    pub linear: LinearPlan,
    /// Problems met while planning (e.g. git unavailable); informational.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonPlan {
    pub alive: bool,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskPlan {
    pub id: String,
    pub key: String,
    pub state: String,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub linear_issue: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TmuxPlan {
    pub installed: bool,
    pub session: String,
    pub socket: Option<String>,
    pub session_exists: bool,
    /// Window ids recorded in the sessions table that still exist in the session.
    pub windows: Vec<String>,
}

/// Where a worktree candidate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeOrigin {
    /// Recorded on a task (`worktree_path`).
    Task,
    /// Registered with git under the worktree root.
    Git,
    /// A directory under the worktree root git no longer knows about.
    Leftover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Remove,
    Keep,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreePlan {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// Key of the task that owns it, when one does.
    pub task: Option<String>,
    pub origin: WorktreeOrigin,
    /// Whether git knows the path as a worktree.
    pub registered: bool,
    pub dirty: bool,
    pub unpushed: u32,
    pub action: Action,
    /// Why it is kept, when it is.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BranchPlan {
    pub branch: String,
    pub action: Action,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DatabasePlan {
    pub path: PathBuf,
    pub counts: ResetCounts,
    /// Whether the `kv` table is wiped too (`--everything`).
    pub clear_kv: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinearPlan {
    pub requested: bool,
    /// Workflow state the issues move back to.
    pub target_state: Option<String>,
    /// `(task key, issue id)` of the open Linear-backed tasks.
    pub issues: Vec<LinearIssuePlan>,
    /// Why nothing will happen, when that is the case.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinearIssuePlan {
    pub key: String,
    pub issue_id: String,
}

/// What actually happened.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ResetResult {
    pub daemon_was_alive: bool,
    pub daemon_stopped: bool,
    pub tmux_windows_killed: usize,
    pub tmux_session_killed: bool,
    pub worktrees_removed: Vec<PathBuf>,
    pub worktrees_kept: Vec<Kept>,
    pub branches_deleted: Vec<String>,
    pub task_dirs_removed: usize,
    pub database: ResetCounts,
    pub linear_reverted: Vec<String>,
    /// Per-item failures; the command exits 1 when this is non-empty.
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Kept {
    pub path: PathBuf,
    pub reason: String,
}

/// Wipe all queue state (see the module docs). Exits 1 when the daemon
/// could not be stopped, when the user declines, or when some item could
/// not be removed; `--dry-run` only prints the plan.
pub fn run(ctx: &mut Context, args: ResetArgs) -> Result<i32> {
    ensure_initialised(ctx)?;
    let cfg = ctx.config_cloned()?;
    let paths = ctx.paths.clone();
    let store = ctx.store()?.clone();
    let linear_key = if args.revert_linear && cfg.linear.enabled { ctx.secrets().get(SecretKind::LinearApiKey)? } else { None };

    let plan = build_plan(&cfg, &paths, &store, &args, linear_key.is_some())?;

    if args.dry_run {
        if ctx.json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print_plan(&plan, &args);
            println!("\nDry run: nothing was changed. Re-run without --dry-run to apply.");
        }
        return Ok(0);
    }

    if !args.yes {
        if !std::io::stdin().is_terminal() {
            bail!("refusing to reset without confirmation: stdin is not a terminal, pass --yes (or --dry-run to preview)");
        }
        print_plan(&plan, &args);
        println!();
        let theme: Box<dyn Theme> = if ctx.color { Box::new(ColorfulTheme::default()) } else { Box::new(SimpleTheme) };
        let ok = Confirm::with_theme(theme.as_ref())
            .with_prompt("Remove everything listed above?")
            .default(false)
            .interact()
            .context("read confirmation")?;
        if !ok {
            println!("aborted; nothing was changed");
            return Ok(1);
        }
    }

    // (a) The daemon must be gone before anything else happens.
    let tick_secs = cfg.scheduler.tick_secs.max(1) as i64;
    let mut result = ResetResult::default();
    if plan.daemon.alive {
        result.daemon_was_alive = true;
        if !ctx.json {
            eprintln!("daemon is running (pid {}); asking it to stop...", plan.daemon.pid.unwrap_or(0));
        }
        if !stop_daemon(&store, tick_secs)? {
            bail!("the daemon is still running; stop it first (`powerqueue stop`, then re-run `powerqueue reset`)");
        }
        result.daemon_stopped = true;
        info!(pid = plan.daemon.pid.unwrap_or(0), "daemon stopped for reset");
    }

    let repo = repo_for(&cfg);
    execute_tmux(&cfg, &plan, &mut result);
    execute_worktrees(repo.as_ref(), &plan, &args, &mut result);
    execute_branches(repo.as_ref(), &plan, &mut result);
    execute_task_dirs(&plan, &mut result);
    execute_linear(&cfg, &plan, linear_key.as_deref(), &mut result);

    // (f) Database last, so a failure above still leaves the records to look at.
    match store.reset(args.everything) {
        Ok(counts) => result.database = counts,
        Err(e) => result.errors.push(format!("database: {e:#}")),
    }

    info!(
        tasks = result.database.tasks,
        worktrees_removed = result.worktrees_removed.len(),
        worktrees_kept = result.worktrees_kept.len(),
        branches_deleted = result.branches_deleted.len(),
        task_dirs_removed = result.task_dirs_removed,
        tmux_session_killed = result.tmux_session_killed,
        linear_reverted = result.linear_reverted.len(),
        everything = args.everything,
        errors = result.errors.len(),
        "queue state reset"
    );

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "plan": plan, "result": result }))?);
    } else {
        print_result(&plan, &result, &args);
    }
    Ok(if result.errors.is_empty() { 0 } else { 1 })
}

// ----------------------------------------------------------------- planning

/// Compute the plan. `have_linear_key` tells whether a Linear API key is
/// available (so `--revert-linear` can be planned without the key itself).
fn build_plan(cfg: &Config, paths: &Paths, store: &Store, args: &ResetArgs, have_linear_key: bool) -> Result<ResetPlan> {
    let mut notes = Vec::new();
    let tasks = store.list_tasks().context("list tasks")?;
    let tick_secs = cfg.scheduler.tick_secs.max(1) as i64;
    let heartbeat = store.daemon_heartbeat()?;
    let daemon =
        DaemonPlan { alive: store.daemon_alive(chrono::Duration::seconds(3 * tick_secs))?, pid: heartbeat.map(|(pid, _)| pid) };

    let mut tasks_by_state = BTreeMap::new();
    for t in &tasks {
        *tasks_by_state.entry(t.state.to_string()).or_insert(0) += 1;
    }
    let task_plans = tasks
        .iter()
        .map(|t| TaskPlan {
            id: t.id.to_string(),
            key: t.key.clone(),
            state: t.state.to_string(),
            worktree_path: t.worktree_path.clone(),
            branch: t.branch.clone(),
            linear_issue: t.linear_identifier().map(str::to_string),
        })
        .collect();

    let tmux = plan_tmux(cfg, store);
    let repo = repo_for(cfg);
    if which::which("git").is_err() {
        notes.push("git is not on PATH: worktrees are treated as plain directories and branches are skipped".to_string());
    } else if repo.is_none() {
        notes.push(format!("repository {} is not a git checkout: worktrees are treated as plain directories", cfg.repo.path));
    }
    let worktree_root = cfg.worktree_root(paths);
    let worktrees = plan_worktrees(cfg, repo.as_ref(), &worktree_root, &tasks, args, &mut notes);
    let branches = if args.delete_branches { plan_branches(cfg, repo.as_ref(), &tasks, &worktrees) } else { Vec::new() };

    let tasks_dir = paths.tasks_dir();
    let task_dirs = list_children(&tasks_dir);

    let database =
        DatabasePlan { path: paths.database(), counts: store.reset_counts().context("count rows")?, clear_kv: args.everything };

    let linear = plan_linear(cfg, &tasks, args, have_linear_key);

    Ok(ResetPlan {
        daemon,
        tasks: task_plans,
        tasks_by_state,
        tmux,
        worktree_root,
        worktrees,
        branches,
        tasks_dir,
        task_dirs,
        database,
        linear,
        notes,
    })
}

/// The repository handle, or `None` when git is missing or the path is not a checkout.
fn repo_for(cfg: &Config) -> Option<Repo> {
    if which::which("git").is_err() {
        return None;
    }
    let repo = Repo::new(cfg.repo_path());
    repo.is_repo().then_some(repo)
}

fn plan_tmux(cfg: &Config, store: &Store) -> TmuxPlan {
    let session = cfg.tmux.session_name.clone();
    let socket = cfg.tmux.socket_name.clone();
    let installed = which::which(&cfg.tmux.binary).is_ok();
    let mut plan =
        TmuxPlan { installed, session: session.clone(), socket: socket.clone(), session_exists: false, windows: Vec::new() };
    if !installed {
        return plan;
    }
    let tmux = Tmux::new(cfg.tmux.binary.clone(), socket);
    plan.session_exists = tmux.has_session(&session).unwrap_or(false);
    if !plan.session_exists {
        return plan;
    }
    // Only windows that still exist in *this* session: window ids restart
    // after a server restart, so a stale `@3` may be someone else's window.
    let live: Vec<String> =
        tmux.list_panes(&session).map(|panes| panes.into_iter().map(|p| p.window_id).collect()).unwrap_or_default();
    let mut windows: Vec<String> = store
        .list_sessions()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.tmux_session == session && live.contains(&s.tmux_window))
        .map(|s| s.tmux_window)
        .collect();
    windows.sort();
    windows.dedup();
    plan.windows = windows;
    plan
}

/// Resolve symlinks where possible so prefix tests compare real locations.
fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// True when `path` is the repository itself or one of its ancestors.
fn touches_repo(path: &Path, repo_canon: &Path) -> bool {
    repo_canon.starts_with(path)
}

fn plan_worktrees(
    cfg: &Config,
    repo: Option<&Repo>,
    root: &Path,
    tasks: &[Task],
    args: &ResetArgs,
    notes: &mut Vec<String>,
) -> Vec<WorktreePlan> {
    let root_canon = canonical(root);
    let repo_canon = canonical(&cfg.repo_path());
    let base = cfg.repo.default_branch.clone().or_else(|| repo.and_then(|r| r.default_branch().ok()));

    // Registered worktrees, keyed by canonical path.
    let registered: Vec<crate::worktree::WorktreeEntry> = match repo {
        Some(r) => match r.list_worktrees() {
            Ok(list) => list.into_iter().filter(|w| !w.bare).collect(),
            Err(e) => {
                notes.push(format!("cannot list git worktrees: {e:#}"));
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let registered_branch = |canon: &Path| -> Option<(bool, Option<String>)> {
        registered.iter().find(|w| canonical(&w.path) == canon).map(|w| (true, w.branch.clone()))
    };

    // Candidate paths in a stable order: tasks first, then git, then leftovers.
    let mut candidates: Vec<(PathBuf, WorktreeOrigin, Option<String>, Option<String>)> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf, origin: WorktreeOrigin, task: Option<String>, branch: Option<String>| {
        let canon = canonical(&path);
        if seen.contains(&canon) {
            return;
        }
        seen.push(canon);
        candidates.push((path, origin, task, branch));
    };
    for t in tasks {
        if let Some(p) = t.worktree_path.as_deref() {
            push(PathBuf::from(p), WorktreeOrigin::Task, Some(t.key.clone()), t.branch.clone());
        }
    }
    for w in &registered {
        if canonical(&w.path).starts_with(&root_canon) {
            push(w.path.clone(), WorktreeOrigin::Git, None, w.branch.clone());
        }
    }
    for child in list_children(root) {
        push(child, WorktreeOrigin::Leftover, None, None);
    }

    let mut out = Vec::new();
    for (path, origin, task, branch) in candidates {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            // Gone from disk: nothing to remove; `git worktree prune` handles the registration.
            continue;
        };
        let canon = canonical(&path);
        let (registered, git_branch) = registered_branch(&canon).unwrap_or((false, None));
        let branch = branch.or(git_branch);
        let mut plan = WorktreePlan {
            path: path.clone(),
            branch: branch.clone(),
            task,
            origin,
            registered,
            dirty: false,
            unpushed: 0,
            action: Action::Remove,
            reason: None,
        };
        let keep = |plan: &mut WorktreePlan, why: String| {
            plan.action = Action::Keep;
            plan.reason = Some(why);
        };

        if meta.file_type().is_symlink() {
            keep(&mut plan, "is a symlink; refusing to follow it".to_string());
        } else if !canon.starts_with(&root_canon) {
            keep(&mut plan, format!("outside the worktree root {}", root.display()));
        } else if canon == root_canon {
            keep(&mut plan, "is the worktree root itself".to_string());
        } else if touches_repo(&canon, &repo_canon) {
            keep(&mut plan, "is the repository itself".to_string());
        } else if !meta.is_dir() {
            keep(&mut plan, "is not a directory".to_string());
        } else if registered {
            let r = repo.expect("registered worktrees imply a repo");
            plan.dirty = r.is_dirty(&path).unwrap_or(true);
            plan.unpushed = match &base {
                Some(base) => r.unpushed_commits(&path, branch.as_deref().unwrap_or("HEAD"), base).unwrap_or(1),
                None => 1,
            };
            if !args.force && (plan.dirty || plan.unpushed > 0) {
                let mut why = Vec::new();
                if plan.dirty {
                    why.push("uncommitted changes".to_string());
                }
                if plan.unpushed > 0 {
                    why.push(format!("{} unpushed commit(s)", plan.unpushed));
                }
                keep(&mut plan, format!("{} (use --force to remove anyway)", why.join(" and ")));
            }
        } else if !args.force && repo.is_none() {
            keep(&mut plan, "cannot check for uncommitted work without git (use --force to remove anyway)".to_string());
        } else if !args.force && path.join(".git").is_dir() {
            keep(&mut plan, "looks like a standalone repository (.git directory), not a worktree (use --force)".to_string());
        }
        out.push(plan);
    }
    out
}

fn plan_branches(cfg: &Config, repo: Option<&Repo>, tasks: &[Task], worktrees: &[WorktreePlan]) -> Vec<BranchPlan> {
    let Some(repo) = repo else { return Vec::new() };
    let default_branch = cfg.repo.default_branch.clone().or_else(|| repo.default_branch().ok());
    let prefix = cfg.repo.branch_template.split('{').next().unwrap_or("").to_string();
    let kept_branches: Vec<&str> =
        worktrees.iter().filter(|w| w.action == Action::Keep).filter_map(|w| w.branch.as_deref()).collect();

    let mut names: Vec<String> = tasks.iter().filter_map(|t| t.branch.clone()).collect();
    names.extend(worktrees.iter().filter(|w| w.origin != WorktreeOrigin::Leftover).filter_map(|w| w.branch.clone()));
    names.sort();
    names.dedup();

    names
        .into_iter()
        .filter(|b| repo.branch_exists(b).unwrap_or(false))
        .map(|branch| {
            let reason = if default_branch.as_deref() == Some(branch.as_str()) {
                Some("is the default branch".to_string())
            } else if !prefix.is_empty() && !branch.starts_with(&prefix) {
                Some(format!("does not match the branch template prefix `{prefix}`"))
            } else if kept_branches.contains(&branch.as_str()) {
                Some("its worktree is kept".to_string())
            } else {
                None
            };
            BranchPlan { branch, action: if reason.is_some() { Action::Keep } else { Action::Remove }, reason }
        })
        .collect()
}

fn plan_linear(cfg: &Config, tasks: &[Task], args: &ResetArgs, have_key: bool) -> LinearPlan {
    let issues: Vec<LinearIssuePlan> = tasks
        .iter()
        .filter(|t| !t.state.is_terminal())
        .filter_map(|t| t.linear_issue_id().map(|id| LinearIssuePlan { key: t.key.clone(), issue_id: id.to_string() }))
        .collect();
    let target_state = cfg.linear.queued_states.first().cloned();
    let note = if !args.revert_linear {
        None
    } else if !cfg.linear.enabled {
        Some("Linear is disabled in config; issues are left as-is".to_string())
    } else if !have_key {
        Some("no Linear API key configured; issues are left as-is".to_string())
    } else if target_state.is_none() {
        Some("linear.queued_states is empty; nowhere to move issues back to".to_string())
    } else if issues.is_empty() {
        Some("no open Linear-backed tasks".to_string())
    } else {
        None
    };
    LinearPlan { requested: args.revert_linear, target_state, issues, note }
}

/// Direct children of `dir`, sorted; empty when it does not exist.
fn list_children(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    out.sort();
    out
}

// ---------------------------------------------------------------- execution

/// Ask the daemon to shut down and wait for its heartbeat to go stale.
/// Returns whether it is gone.
fn stop_daemon(store: &Store, tick_secs: i64) -> Result<bool> {
    store.enqueue_command(&DaemonCommand::Shutdown).context("queue shutdown command")?;
    let max_age = chrono::Duration::seconds(3 * tick_secs);
    let wait = Duration::from_secs(DAEMON_STOP_WAIT_SECS.max(3 * tick_secs as u64 + 5));
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if !store.daemon_alive(max_age)? {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(!store.daemon_alive(max_age)?)
}

fn execute_tmux(cfg: &Config, plan: &ResetPlan, result: &mut ResetResult) {
    if !plan.tmux.installed || !plan.tmux.session_exists {
        return;
    }
    let tmux = Tmux::new(cfg.tmux.binary.clone(), cfg.tmux.socket_name.clone());
    for w in &plan.tmux.windows {
        match tmux.kill_window(w) {
            Ok(()) => result.tmux_windows_killed += 1,
            Err(e) => warn!(window = %w, error = %format!("{e:#}"), "tmux window already gone or kill failed"),
        }
    }
    match tmux.kill_session(&plan.tmux.session) {
        Ok(()) => result.tmux_session_killed = true,
        Err(e) => {
            if tmux.has_session(&plan.tmux.session).unwrap_or(false) {
                result.errors.push(format!("tmux session `{}`: {e:#}", plan.tmux.session));
            } else {
                result.tmux_session_killed = true;
            }
        }
    }
}

fn execute_worktrees(repo: Option<&Repo>, plan: &ResetPlan, args: &ResetArgs, result: &mut ResetResult) {
    for wt in &plan.worktrees {
        if wt.action == Action::Keep {
            result.worktrees_kept.push(Kept { path: wt.path.clone(), reason: wt.reason.clone().unwrap_or_default() });
            info!(path = %wt.path.display(), reason = wt.reason.as_deref().unwrap_or(""), "worktree kept");
            continue;
        }
        // Re-check the invariants right before deleting: the plan is advisory.
        let canon = canonical(&wt.path);
        if !canon.starts_with(canonical(&plan.worktree_root)) || canon == canonical(&plan.worktree_root) {
            result.errors.push(format!("{}: refusing to remove a path outside the worktree root", wt.path.display()));
            continue;
        }
        let outcome = match (wt.registered, repo) {
            (true, Some(r)) => r.remove_worktree(&wt.path, args.force),
            _ => Ok(()),
        }
        .and_then(|()| {
            if wt.path.exists() {
                std::fs::remove_dir_all(&wt.path).with_context(|| format!("remove directory {}", wt.path.display()))
            } else {
                Ok(())
            }
        });
        match outcome {
            Ok(()) => {
                info!(path = %wt.path.display(), branch = wt.branch.as_deref().unwrap_or(""), "worktree removed");
                result.worktrees_removed.push(wt.path.clone());
            }
            Err(e) => result.errors.push(format!("worktree {}: {e:#}", wt.path.display())),
        }
    }
    if let Some(r) = repo
        && let Err(e) = r.prune_worktrees()
    {
        result.errors.push(format!("git worktree prune: {e:#}"));
    }
}

fn execute_branches(repo: Option<&Repo>, plan: &ResetPlan, result: &mut ResetResult) {
    let Some(repo) = repo else { return };
    for b in plan.branches.iter().filter(|b| b.action == Action::Remove) {
        match repo.delete_branch(&b.branch, true) {
            Ok(()) => result.branches_deleted.push(b.branch.clone()),
            Err(e) => result.errors.push(format!("branch {}: {e:#}", b.branch)),
        }
    }
}

fn execute_task_dirs(plan: &ResetPlan, result: &mut ResetResult) {
    let dir_canon = canonical(&plan.tasks_dir);
    for entry in &plan.task_dirs {
        let Ok(meta) = std::fs::symlink_metadata(entry) else { continue };
        let outcome = if meta.file_type().is_symlink() || !meta.is_dir() {
            // Remove the link itself, never what it points to.
            std::fs::remove_file(entry)
        } else if canonical(entry).starts_with(&dir_canon) {
            std::fs::remove_dir_all(entry)
        } else {
            result.errors.push(format!("{}: refusing to remove a path outside {}", entry.display(), plan.tasks_dir.display()));
            continue;
        };
        match outcome {
            Ok(()) => result.task_dirs_removed += 1,
            Err(e) => result.errors.push(format!("task dir {}: {e}", entry.display())),
        }
    }
}

fn execute_linear(cfg: &Config, plan: &ResetPlan, key: Option<&str>, result: &mut ResetResult) {
    if !plan.linear.requested || plan.linear.note.is_some() {
        return;
    }
    let (Some(key), Some(target)) = (key, plan.linear.target_state.as_deref()) else { return };
    let client = match LinearClient::new(&cfg.linear.endpoint, key) {
        Ok(c) => c,
        Err(e) => {
            result.errors.push(format!("linear: {e:#}"));
            return;
        }
    };
    let rt = match super::runtime() {
        Ok(rt) => rt,
        Err(e) => {
            result.errors.push(format!("linear: {e:#}"));
            return;
        }
    };
    for issue in &plan.linear.issues {
        match rt.block_on(client.set_state(&issue.issue_id, target)) {
            Ok(()) => {
                info!(task = %issue.key, state = target, "Linear issue moved back to the queue");
                result.linear_reverted.push(issue.key.clone());
            }
            Err(e) => result.errors.push(format!("linear {}: {e:#}", issue.key)),
        }
    }
}

// ------------------------------------------------------------------ output

fn bold(s: &str) -> String {
    s.if_supports_color(Stream::Stdout, |t| t.bold()).to_string()
}

fn print_plan(plan: &ResetPlan, args: &ResetArgs) {
    println!("{}", bold("powerqueue reset will remove:"));

    let daemon = match (plan.daemon.alive, plan.daemon.pid) {
        (true, Some(pid)) => format!("running (pid {pid}); it will be asked to stop first"),
        (true, None) => "running; it will be asked to stop first".to_string(),
        _ => "not running".to_string(),
    };
    println!("  {:<11}{daemon}", "daemon");

    let states: Vec<String> = plan.tasks_by_state.iter().map(|(s, n)| format!("{s} {n}")).collect();
    println!(
        "  {:<11}{} ({})",
        "tasks",
        plan.tasks.len(),
        if states.is_empty() { "none".to_string() } else { states.join(", ") }
    );
    for t in &plan.tasks {
        println!("    {:<12} {}{}", t.key, t.state, t.linear_issue.as_deref().map(|_| " (Linear)").unwrap_or(""));
    }

    let tmux = if !plan.tmux.installed {
        "tmux is not installed; skipped".to_string()
    } else if !plan.tmux.session_exists {
        format!("session `{}` does not exist; nothing to do", plan.tmux.session)
    } else {
        format!(
            "kill {} task window(s) and session `{}`{}",
            plan.tmux.windows.len(),
            plan.tmux.session,
            plan.tmux.socket.as_deref().map(|s| format!(" (socket {s})")).unwrap_or_default()
        )
    };
    println!("  {:<11}{tmux}", "tmux");

    println!("  {:<11}{} under {}", "worktrees", plan.worktrees.len(), plan.worktree_root.display());
    for w in &plan.worktrees {
        let what = match (w.branch.as_deref(), w.task.as_deref()) {
            (Some(b), Some(t)) => format!(" ({b}, task {t})"),
            (Some(b), None) => format!(" ({b})"),
            (None, Some(t)) => format!(" (task {t})"),
            (None, None) => {
                if w.origin == WorktreeOrigin::Leftover {
                    " (leftover directory)".to_string()
                } else {
                    String::new()
                }
            }
        };
        match w.action {
            Action::Remove => println!("    remove  {}{what}", w.path.display()),
            Action::Keep => println!("    keep    {}{what}: {}", w.path.display(), w.reason.as_deref().unwrap_or("")),
        }
    }

    if args.delete_branches {
        println!("  {:<11}{} local branch(es)", "branches", plan.branches.iter().filter(|b| b.action == Action::Remove).count());
        for b in &plan.branches {
            match b.action {
                Action::Remove => println!("    delete  {}", b.branch),
                Action::Keep => println!("    keep    {}: {}", b.branch, b.reason.as_deref().unwrap_or("")),
            }
        }
    } else {
        println!("  {:<11}kept (pass --delete-branches to delete the local task branches)", "branches");
    }

    println!("  {:<11}{} under {}", "task dirs", plan.task_dirs.len(), plan.tasks_dir.display());

    let c = &plan.database.counts;
    println!(
        "  {:<11}{}: tasks {}, sessions {}, usage {}, resource samples {}, events {}, commands {}, hook events {}, jev scores {}; kv {}",
        "database",
        plan.database.path.display(),
        c.tasks,
        c.sessions,
        c.usage,
        c.resource_samples,
        c.events,
        c.commands,
        c.hook_events,
        c.jev_scores,
        if plan.database.clear_kv {
            format!("{} row(s) cleared (--everything)", c.kv)
        } else {
            format!("kept ({} row(s): usage readings, cooldowns, probes, pause; --everything clears it)", c.kv)
        }
    );

    let linear = if !plan.linear.requested {
        "issues left as-is (pass --revert-linear to move open ones back to the queue)".to_string()
    } else if let Some(note) = &plan.linear.note {
        note.clone()
    } else {
        format!(
            "move {} issue(s) back to `{}`: {}",
            plan.linear.issues.len(),
            plan.linear.target_state.as_deref().unwrap_or("?"),
            plan.linear.issues.iter().map(|i| i.key.as_str()).collect::<Vec<_>>().join(", ")
        )
    };
    println!("  {:<11}{linear}", "linear");

    for n in &plan.notes {
        print_warning(n);
    }
}

fn print_result(plan: &ResetPlan, result: &ResetResult, args: &ResetArgs) {
    println!("{}", bold("powerqueue reset done"));
    println!("  {:<11}{}", "daemon", if result.daemon_stopped { "stopped" } else { "was not running" });
    let tmux = if !plan.tmux.installed {
        "not installed; skipped".to_string()
    } else if result.tmux_session_killed {
        format!("killed {} window(s) and session `{}`", result.tmux_windows_killed, plan.tmux.session)
    } else {
        "no session to kill".to_string()
    };
    println!("  {:<11}{tmux}", "tmux");
    println!("  {:<11}removed {}, kept {}", "worktrees", result.worktrees_removed.len(), result.worktrees_kept.len());
    for k in &result.worktrees_kept {
        println!("    kept    {}: {}", k.path.display(), k.reason);
    }
    if args.delete_branches {
        println!("  {:<11}deleted {}", "branches", result.branches_deleted.len());
    }
    println!("  {:<11}removed {}", "task dirs", result.task_dirs_removed);
    let c = &result.database;
    println!(
        "  {:<11}removed {} row(s): tasks {}, sessions {}, usage {}, resource samples {}, events {}, commands {}, hook events {}, jev scores {}, kv {}",
        "database",
        c.total(),
        c.tasks,
        c.sessions,
        c.usage,
        c.resource_samples,
        c.events,
        c.commands,
        c.hook_events,
        c.jev_scores,
        c.kv
    );
    if plan.linear.requested {
        println!(
            "  {:<11}moved {} issue(s) back to `{}`{}",
            "linear",
            result.linear_reverted.len(),
            plan.linear.target_state.as_deref().unwrap_or("?"),
            plan.linear.note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default()
        );
    }
    if !result.errors.is_empty() {
        println!("  {:<11}{}", "errors", result.errors.len());
        for e in &result.errors {
            println!("    {}", e.if_supports_color(Stream::Stdout, |t| t.red()));
        }
    }
    println!(
        "\nKept: config.toml, secrets, PRIORITY.md and logs.{}",
        if plan.linear.requested { "" } else { " Linear issues were left as-is (use --revert-linear to re-queue them)." }
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskSource;

    #[test]
    fn linear_plan_explains_why_nothing_happens() {
        let mut cfg = Config::default();
        let mut t = Task::new(
            "ENG-1",
            "t",
            TaskSource::Linear {
                issue_id: "id-1".into(),
                identifier: "ENG-1".into(),
                url: String::new(),
                team_key: "ENG".into(),
            },
        );
        let mut done = t.clone();
        done.key = "ENG-2".into();
        done.state = crate::domain::TaskState::Completed;
        t.state = crate::domain::TaskState::Running;
        let tasks = vec![t, done];

        let off = ResetArgs::default();
        let p = plan_linear(&cfg, &tasks, &off, true);
        assert!(!p.requested && p.note.is_none());
        assert_eq!(p.issues.len(), 1, "only open tasks are candidates");

        let on = ResetArgs { revert_linear: true, ..ResetArgs::default() };
        assert!(plan_linear(&cfg, &tasks, &on, false).note.as_deref().unwrap().contains("no Linear API key"));
        assert!(plan_linear(&cfg, &tasks, &on, true).note.is_none());
        cfg.linear.queued_states.clear();
        assert!(plan_linear(&cfg, &tasks, &on, true).note.as_deref().unwrap().contains("queued_states"));
        cfg.linear.enabled = false;
        assert!(plan_linear(&cfg, &tasks, &on, true).note.as_deref().unwrap().contains("disabled"));
    }

    #[test]
    fn touches_repo_matches_repo_and_ancestors_only() {
        let repo = Path::new("/home/u/code/app");
        assert!(touches_repo(Path::new("/home/u/code/app"), repo));
        assert!(touches_repo(Path::new("/home/u"), repo));
        assert!(!touches_repo(Path::new("/home/u/code/app-wt/x"), repo));
        assert!(!touches_repo(Path::new("/home/u/code/other"), repo));
    }
}
