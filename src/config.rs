//! Configuration: a global `config.toml` plus an optional per-repository
//! `.powerqueue.toml` that can override runtime, cleanup and Claude settings.
//!
//! Every field has a sensible default so a freshly `init`ed install works with
//! a nearly empty file. Validation happens in [`Config::validate`] and is
//! surfaced by `powerqueue doctor`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::domain::{Criticality, ModelTier};
use crate::paths::{Paths, expand_tilde};

/// Name of the per-repository override file.
pub const REPO_CONFIG_FILE: &str = ".powerqueue.toml";

/// Top-level configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub repo: RepoConfig,
    pub linear: LinearConfig,
    pub priority: PriorityConfig,
    pub scheduler: SchedulerConfig,
    pub claude: ClaudeConfig,
    pub budget: BudgetConfig,
    pub cleanup: CleanupConfig,
    pub tmux: TmuxConfig,
    pub logging: LoggingConfig,
}

/// The repository tasks are worked on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoConfig {
    /// Path to the main checkout. Worktrees are created from it.
    pub path: String,
    /// Branch new worktrees start from. `None` = detect (`origin/HEAD`, then `main`/`master`).
    pub default_branch: Option<String>,
    /// Where worktrees live. Defaults to `<data_dir>/worktrees/<repo-name>`.
    pub worktree_root: Option<String>,
    /// Template for branch names. `{key}` = task key slug, `{id}` = short task id.
    pub branch_template: String,
    /// `git fetch` before creating a worktree.
    pub fetch_before_start: bool,
    /// Commands run inside a fresh worktree before Claude starts (e.g. `npm ci`).
    /// Each entry is run with `sh -c`.
    pub setup: Vec<String>,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            path: String::new(),
            default_branch: None,
            worktree_root: None,
            branch_template: "pq/{key}".to_string(),
            fetch_before_start: true,
            setup: Vec::new(),
        }
    }
}

/// Linear integration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinearConfig {
    pub enabled: bool,
    /// Team keys to pull issues from (e.g. `["ENG"]`). Empty = all teams visible to the key.
    pub team_keys: Vec<String>,
    /// Only issues assigned to this user id, or `"me"`. `None` = any assignee.
    pub assignee: Option<String>,
    /// Workflow state *names* considered queued for the agent.
    pub queued_states: Vec<String>,
    /// Issues must carry at least one of these labels (empty = no label filter).
    pub required_labels: Vec<String>,
    /// Issues with any of these labels are ignored.
    pub excluded_labels: Vec<String>,
    /// State to move an issue to when a session starts.
    pub in_progress_state: Option<String>,
    /// State to move an issue to when the task completes.
    pub done_state: Option<String>,
    /// State to move an issue to when the task fails permanently or is blocked.
    pub blocked_state: Option<String>,
    /// Post progress comments on the issue.
    pub post_comments: bool,
    pub poll_interval_secs: u64,
    /// Cap on issues fetched per poll.
    pub max_issues: u32,
    /// GraphQL endpoint; overridable for tests.
    pub endpoint: String,
}

impl Default for LinearConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            team_keys: Vec::new(),
            assignee: None,
            queued_states: vec!["Todo".to_string()],
            required_labels: Vec::new(),
            excluded_labels: vec!["no-agent".to_string()],
            in_progress_state: Some("In Progress".to_string()),
            done_state: Some("In Review".to_string()),
            blocked_state: None,
            post_comments: true,
            poll_interval_secs: 60,
            max_issues: 100,
            endpoint: "https://api.linear.app/graphql".to_string(),
        }
    }
}

/// Where priority rules come from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PriorityConfig {
    /// Path to the rules file. Defaults to `<config_dir>/PRIORITY.md`.
    pub file: Option<String>,
    /// Watch the file and re-score tasks on change.
    pub live_reload: bool,
    /// Older tasks slowly gain score so nothing starves: points per hour.
    pub age_boost_per_hour: f64,
    pub jev: JevConfig,
}

impl Default for PriorityConfig {
    fn default() -> Self {
        Self { file: None, live_reload: true, age_boost_per_hour: 2.0, jev: JevConfig::default() }
    }
}

/// Optional Jev (TypeSafe "System One") scoring of tickets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JevConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub model: String,
    /// Re-score an issue when its title/description changes; otherwise cache.
    pub rescore_on_change: bool,
    /// How much Jev's score (0..1 across the rubric) contributes, in points.
    pub weight: f64,
}

impl Default for JevConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "https://api.typesafe.ai/v1/systemone".to_string(),
            model: "jev-latest".to_string(),
            rescore_on_change: true,
            weight: 300.0,
        }
    }
}

/// Daemon behaviour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulerConfig {
    pub max_concurrent: u32,
    pub max_attempts: u32,
    /// Main loop period.
    pub tick_secs: u64,
    /// A session that ended its turn without a completion marker and has been
    /// silent this long is nudged once, then marked `needs_attention`.
    pub idle_timeout_secs: u64,
    /// No transcript growth / hook events for this long while "running" = hung.
    pub stale_session_secs: u64,
    /// Backoff after crash, per attempt (last value repeats).
    pub restart_backoff_secs: Vec<u64>,
    /// Hard cap on wall-clock per attempt; 0 disables.
    pub max_session_secs: u64,
    /// How often to sample CPU/RSS of sessions.
    pub resource_sample_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 2,
            max_attempts: 3,
            tick_secs: 5,
            idle_timeout_secs: 600,
            stale_session_secs: 1800,
            restart_backoff_secs: vec![30, 120, 600],
            max_session_secs: 4 * 3600,
            resource_sample_secs: 30,
        }
    }
}

impl SchedulerConfig {
    pub fn backoff_for_attempt(&self, attempt: u32) -> u64 {
        if self.restart_backoff_secs.is_empty() {
            return 60;
        }
        let idx = (attempt.max(1) as usize - 1).min(self.restart_backoff_secs.len() - 1);
        self.restart_backoff_secs[idx]
    }
}

/// How Claude Code is launched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeConfig {
    pub binary: String,
    /// `--permission-mode` value. `acceptEdits` is the safe default (other
    /// tools still prompt); `auto` lets Claude Code's classifier approve
    /// routine commands for unattended runs; `bypassPermissions` never asks.
    pub permission_mode: String,
    /// `--effort` value, if any.
    pub effort: Option<String>,
    /// Extra flags appended verbatim.
    pub extra_args: Vec<String>,
    /// Extra `--allowedTools` patterns.
    pub allowed_tools: Vec<String>,
    /// Appended to the system prompt for every task.
    pub append_system_prompt: Option<String>,
    /// Fallback model chain passed as `--fallback-model`.
    pub fallback_models: Vec<String>,
    /// Mark the repository and each worktree as trusted in Claude Code's
    /// `~/.claude.json` before launching, so sessions never stop at the
    /// workspace-trust dialog. Disable if you manage trust yourself.
    pub trust_workspace: bool,
    /// Environment variables for the session.
    pub env: BTreeMap<String, String>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            binary: "claude".to_string(),
            permission_mode: "acceptEdits".to_string(),
            effort: None,
            extra_args: Vec::new(),
            allowed_tools: Vec::new(),
            append_system_prompt: None,
            fallback_models: Vec::new(),
            trust_workspace: true,
            env: BTreeMap::new(),
        }
    }
}

/// Model-usage pacing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    /// Length of the usage period in hours (Claude subscriptions reset weekly).
    pub period_hours: u64,
    /// RFC 3339 instant at which a period starts; later periods repeat every
    /// `period_hours`. Learn it from `/usage` in Claude Code and set it with
    /// `powerqueue budget set-reset <time>`.
    pub period_anchor: Option<String>,
    /// Rolling short window (hours) that also limits usage. 5h for Claude subscriptions.
    pub window_hours: u64,
    /// Total weighted tokens the period may consume across all models.
    pub period_weighted_tokens: u64,
    /// Weighted tokens the rolling window may consume.
    pub window_weighted_tokens: u64,
    /// Per-model policy.
    pub models: BTreeMap<ModelTier, ModelBudget>,
    /// Default model when nothing else applies.
    pub default_model: ModelTier,
    /// Model used for `low` tasks.
    pub low_model: ModelTier,
    /// Keep this fraction of the period budget unspent as a safety margin.
    pub safety_margin: f64,
    /// Fraction of the period after which unused reserved capacity is released ("end game").
    pub endgame_fraction: f64,
    /// Pause scheduling when a `rate_limit` error is reported, for this many minutes
    /// (unless the period/window reset comes sooner).
    pub rate_limit_cooldown_mins: u64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        let mut models = BTreeMap::new();
        models.insert(
            ModelTier::Fable,
            ModelBudget {
                share: 0.25,
                min_criticality: Criticality::Critical,
                relax_after_fraction: 0.5,
                weight: 5.0,
                enabled: true,
            },
        );
        models.insert(
            ModelTier::Opus,
            ModelBudget {
                share: 0.35,
                min_criticality: Criticality::High,
                relax_after_fraction: 0.3,
                weight: 3.0,
                enabled: true,
            },
        );
        models.insert(
            ModelTier::Sonnet,
            ModelBudget { share: 0.35, min_criticality: Criticality::Low, relax_after_fraction: 0.0, weight: 1.0, enabled: true },
        );
        models.insert(
            ModelTier::Haiku,
            ModelBudget { share: 0.05, min_criticality: Criticality::Low, relax_after_fraction: 0.0, weight: 0.2, enabled: true },
        );
        Self {
            period_hours: 24 * 7,
            period_anchor: None,
            window_hours: 5,
            period_weighted_tokens: 80_000_000,
            window_weighted_tokens: 12_000_000,
            models,
            default_model: ModelTier::Sonnet,
            low_model: ModelTier::Sonnet,
            safety_margin: 0.05,
            endgame_fraction: 0.8,
            rate_limit_cooldown_mins: 30,
        }
    }
}

/// Policy for one model tier.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelBudget {
    /// Fraction of `period_weighted_tokens` this tier may use.
    pub share: f64,
    /// Tasks must be at least this critical to get this tier early in the period.
    pub min_criticality: Criticality,
    /// After this fraction of the period has elapsed, if the tier is under-spent,
    /// tasks one level below `min_criticality` may use it as well (and two levels
    /// below once in the end game).
    pub relax_after_fraction: f64,
    /// Cost weight relative to Sonnet = 1.0.
    pub weight: f64,
    pub enabled: bool,
}

impl Default for ModelBudget {
    fn default() -> Self {
        Self { share: 0.25, min_criticality: Criticality::Low, relax_after_fraction: 0.0, weight: 1.0, enabled: true }
    }
}

/// What happens when a task completes or fails.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanupConfig {
    /// Remove the worktree after completion.
    pub remove_worktree: bool,
    /// Push the branch before removing the worktree (so work is not lost).
    pub push_branch: bool,
    /// Delete the local branch after push/completion.
    pub delete_branch: bool,
    /// Keep worktrees of failed tasks for inspection.
    pub keep_failed: bool,
    /// Commands run in the worktree before removal (`sh -c`).
    pub run: Vec<String>,
    /// Kill the tmux window after completion (otherwise it stays with the final output).
    pub close_tmux_window: bool,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            remove_worktree: true,
            push_branch: true,
            delete_branch: false,
            keep_failed: true,
            run: Vec::new(),
            close_tmux_window: true,
        }
    }
}

/// tmux integration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TmuxConfig {
    pub binary: String,
    /// tmux session that hosts one window per task.
    pub session_name: String,
    /// Optional tmux socket name (`-L`).
    pub socket_name: Option<String>,
    /// Keep dead panes visible so crashes can be inspected.
    pub remain_on_exit: bool,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self { binary: "tmux".to_string(), session_name: "powerqueue".to_string(), socket_name: None, remain_on_exit: true }
    }
}

/// Logging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// `tracing` filter, e.g. `info,powerqueue=debug`.
    pub level: String,
    /// Keep this many rotated daily log files.
    pub keep_days: u32,
    /// Write JSON lines (true) or human text (false) to the log file.
    pub json: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self { level: "info".to_string(), keep_days: 14, json: true }
    }
}

/// Per-repository overrides (`.powerqueue.toml` in the repo root).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RepoOverrides {
    pub setup: Option<Vec<String>>,
    pub default_branch: Option<String>,
    pub branch_template: Option<String>,
    pub cleanup: Option<CleanupOverrides>,
    pub claude: Option<ClaudeOverrides>,
    /// Extra instructions appended to every task prompt for this repo.
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CleanupOverrides {
    pub remove_worktree: Option<bool>,
    pub push_branch: Option<bool>,
    pub delete_branch: Option<bool>,
    pub keep_failed: Option<bool>,
    pub run: Option<Vec<String>>,
    pub close_tmux_window: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeOverrides {
    pub permission_mode: Option<String>,
    pub effort: Option<String>,
    pub extra_args: Option<Vec<String>>,
    pub allowed_tools: Option<Vec<String>>,
    pub append_system_prompt: Option<String>,
}

impl Config {
    /// Load `config.toml` from the resolved paths. A missing file is an error
    /// with a hint to run `powerqueue init`.
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("cannot read {} (run `powerqueue init` first)", file.display()))?;
        Self::from_toml(&text).with_context(|| format!("invalid config {}", file.display()))
    }

    /// Load if present, otherwise defaults (for commands that must work before init).
    pub fn load_or_default(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        if file.exists() { Self::load(paths) } else { Ok(Self::default()) }
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        Ok(cfg)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        std::fs::create_dir_all(&paths.config_dir)?;
        let file = paths.config_file();
        let text = self.to_toml()?;
        write_private(&file, text.as_bytes())?;
        Ok(())
    }

    /// Apply a repository's `.powerqueue.toml`, if present.
    pub fn apply_repo_overrides(&mut self, repo_path: &Path) -> Result<Option<PathBuf>> {
        let file = repo_path.join(REPO_CONFIG_FILE);
        if !file.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&file)?;
        let ov: RepoOverrides = toml::from_str(&text).with_context(|| format!("invalid {}", file.display()))?;
        self.merge_overrides(ov);
        Ok(Some(file))
    }

    pub fn merge_overrides(&mut self, ov: RepoOverrides) {
        if let Some(setup) = ov.setup {
            self.repo.setup = setup;
        }
        if ov.default_branch.is_some() {
            self.repo.default_branch = ov.default_branch;
        }
        if let Some(t) = ov.branch_template {
            self.repo.branch_template = t;
        }
        if let Some(c) = ov.cleanup {
            if let Some(v) = c.remove_worktree {
                self.cleanup.remove_worktree = v;
            }
            if let Some(v) = c.push_branch {
                self.cleanup.push_branch = v;
            }
            if let Some(v) = c.delete_branch {
                self.cleanup.delete_branch = v;
            }
            if let Some(v) = c.keep_failed {
                self.cleanup.keep_failed = v;
            }
            if let Some(v) = c.run {
                self.cleanup.run = v;
            }
            if let Some(v) = c.close_tmux_window {
                self.cleanup.close_tmux_window = v;
            }
        }
        if let Some(c) = ov.claude {
            if let Some(v) = c.permission_mode {
                self.claude.permission_mode = v;
            }
            if c.effort.is_some() {
                self.claude.effort = c.effort;
            }
            if let Some(v) = c.extra_args {
                self.claude.extra_args = v;
            }
            if let Some(v) = c.allowed_tools {
                self.claude.allowed_tools = v;
            }
            if let Some(v) = c.append_system_prompt {
                self.claude.append_system_prompt = Some(match &self.claude.append_system_prompt {
                    Some(existing) => format!("{existing}\n\n{v}"),
                    None => v,
                });
            }
        }
        if let Some(instr) = ov.instructions {
            self.claude.append_system_prompt = Some(match &self.claude.append_system_prompt {
                Some(existing) => format!("{existing}\n\n{instr}"),
                None => instr,
            });
        }
    }

    /// Absolute repository path.
    pub fn repo_path(&self) -> PathBuf {
        expand_tilde(&self.repo.path)
    }

    /// Where worktrees for this repo live.
    pub fn worktree_root(&self, paths: &Paths) -> PathBuf {
        match &self.repo.worktree_root {
            Some(p) => expand_tilde(p),
            None => {
                let name =
                    self.repo_path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "repo".to_string());
                paths.worktrees_dir().join(name)
            }
        }
    }

    pub fn priority_file(&self, paths: &Paths) -> PathBuf {
        match &self.priority.file {
            Some(p) => expand_tilde(p),
            None => paths.priority_file(),
        }
    }

    /// Structural validation; returns a list of human-readable problems.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.repo.path.trim().is_empty() {
            problems.push("repo.path is empty".to_string());
        }
        if !self.repo.branch_template.contains("{key}") && !self.repo.branch_template.contains("{id}") {
            problems.push("repo.branch_template must contain {key} or {id}".to_string());
        }
        if self.scheduler.max_concurrent == 0 {
            problems.push("scheduler.max_concurrent must be >= 1".to_string());
        }
        if self.scheduler.max_attempts == 0 {
            problems.push("scheduler.max_attempts must be >= 1".to_string());
        }
        if self.scheduler.tick_secs == 0 {
            problems.push("scheduler.tick_secs must be >= 1".to_string());
        }
        if self.budget.period_hours == 0 || self.budget.window_hours == 0 {
            problems.push("budget.period_hours and budget.window_hours must be >= 1".to_string());
        }
        if self.budget.window_hours > self.budget.period_hours {
            problems.push("budget.window_hours cannot exceed budget.period_hours".to_string());
        }
        let share: f64 = self.budget.models.values().filter(|m| m.enabled).map(|m| m.share).sum();
        if share > 1.0 + 1e-6 {
            problems.push(format!("budget.models shares sum to {share:.2} (> 1.0)"));
        }
        if !(0.0..=0.5).contains(&self.budget.safety_margin) {
            problems.push("budget.safety_margin must be between 0 and 0.5".to_string());
        }
        if !(0.0..=1.0).contains(&self.budget.endgame_fraction) {
            problems.push("budget.endgame_fraction must be between 0 and 1".to_string());
        }
        if let Some(anchor) = &self.budget.period_anchor
            && chrono::DateTime::parse_from_rfc3339(anchor).is_err()
        {
            problems.push(format!("budget.period_anchor `{anchor}` is not RFC 3339"));
        }
        for (tier, m) in &self.budget.models {
            if !(0.0..=1.0).contains(&m.share) {
                problems.push(format!("budget.models.{tier}.share must be between 0 and 1"));
            }
            if !(0.0..=1.0).contains(&m.relax_after_fraction) {
                problems.push(format!("budget.models.{tier}.relax_after_fraction must be between 0 and 1"));
            }
        }
        let allowed_modes = ["default", "manual", "acceptEdits", "plan", "auto", "dontAsk", "bypassPermissions"];
        if !allowed_modes.contains(&self.claude.permission_mode.as_str()) {
            problems.push(format!(
                "claude.permission_mode `{}` is not one of {}",
                self.claude.permission_mode,
                allowed_modes.join("|")
            ));
        }
        if self.linear.enabled && self.linear.queued_states.is_empty() {
            problems.push("linear.queued_states is empty; no issues would ever be picked up".to_string());
        }
        problems
    }

    /// `validate` as a hard error.
    pub fn ensure_valid(&self) -> Result<()> {
        let problems = self.validate();
        if problems.is_empty() { Ok(()) } else { bail!("invalid configuration:\n  - {}", problems.join("\n  - ")) }
    }
}

/// Write a file readable only by the owner (0600 on unix).
pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// The commented template written by `powerqueue init`.
pub fn config_template(repo_path: &str, team_keys: &[String]) -> String {
    let mut cfg = Config::default();
    cfg.repo.path = repo_path.to_string();
    cfg.linear.team_keys = team_keys.to_vec();
    let body = cfg.to_toml().unwrap_or_default();
    format!(
        "# powerqueue configuration\n\
         # Docs: https://github.com/aleandros/powerqueue#configuration\n\
         # Every key is optional; defaults are shown. Edit and run `powerqueue doctor`.\n\n{body}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_except_repo_path() {
        let cfg = Config::default();
        let problems = cfg.validate();
        assert_eq!(problems, vec!["repo.path is empty".to_string()]);
    }

    #[test]
    fn round_trips_through_toml() {
        let mut cfg = Config::default();
        cfg.repo.path = "/tmp/repo".into();
        cfg.linear.team_keys = vec!["ENG".into()];
        let text = cfg.to_toml().unwrap();
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Config::from_toml("[repo]\npath='/x'\nbogus = 1\n").unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn partial_config_uses_defaults() {
        let cfg = Config::from_toml("[repo]\npath = '/x'\n[scheduler]\nmax_concurrent = 4\n").unwrap();
        assert_eq!(cfg.scheduler.max_concurrent, 4);
        assert_eq!(cfg.scheduler.max_attempts, 3);
        assert_eq!(cfg.budget.models[&ModelTier::Fable].min_criticality, Criticality::Critical);
    }

    #[test]
    fn repo_overrides_merge() {
        let mut cfg = Config::default();
        cfg.claude.append_system_prompt = Some("base".into());
        let ov: RepoOverrides = toml::from_str(
            "setup = ['npm ci']\ninstructions = 'Run tests'\n[cleanup]\nremove_worktree = false\n[claude]\npermission_mode = 'plan'\n",
        )
        .unwrap();
        cfg.merge_overrides(ov);
        assert_eq!(cfg.repo.setup, vec!["npm ci".to_string()]);
        assert!(!cfg.cleanup.remove_worktree);
        assert_eq!(cfg.claude.permission_mode, "plan");
        assert_eq!(cfg.claude.append_system_prompt.as_deref(), Some("base\n\nRun tests"));
    }

    #[test]
    fn validation_catches_bad_values() {
        let mut cfg = Config::default();
        cfg.repo.path = "/x".into();
        cfg.scheduler.max_concurrent = 0;
        cfg.claude.permission_mode = "yolo".into();
        cfg.budget.models.get_mut(&ModelTier::Fable).unwrap().share = 0.9;
        let problems = cfg.validate();
        assert!(problems.iter().any(|p| p.contains("max_concurrent")));
        assert!(problems.iter().any(|p| p.contains("permission_mode")));
        assert!(problems.iter().any(|p| p.contains("shares sum")));
    }

    #[test]
    fn backoff_clamps_to_last_value() {
        let s = SchedulerConfig::default();
        assert_eq!(s.backoff_for_attempt(1), 30);
        assert_eq!(s.backoff_for_attempt(2), 120);
        assert_eq!(s.backoff_for_attempt(99), 600);
    }

    #[test]
    fn template_parses() {
        let text = config_template("/tmp/repo", &["ENG".to_string()]);
        let cfg = Config::from_toml(&text).unwrap();
        assert_eq!(cfg.repo.path, "/tmp/repo");
    }
}
