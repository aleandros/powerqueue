//! Configuration: a global `config.toml` plus an optional per-repository
//! `.powerqueue.toml` that can override runtime, cleanup and agent settings.
//!
//! Every field has a sensible default so a freshly `init`ed install works with
//! a nearly empty file. Validation happens in [`Config::validate`] and is
//! surfaced by `powerqueue doctor`.
//!
//! Budgets are per provider (`[budget.providers.claude]`, `.codex`,
//! `.gemini`); the flat `[budget]` keys and `[budget.models.<tier>]` tables of
//! older versions are moved under `budget.providers.claude` on load by
//! [`migrate_legacy_budget`] and reported by [`Config::deprecations`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::domain::{Criticality, ModelTier, Provider};
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
    pub prompt: PromptConfig,
    pub scheduler: SchedulerConfig,
    pub claude: ClaudeConfig,
    pub codex: CodexConfig,
    pub gemini: GeminiConfig,
    pub budget: BudgetConfig,
    pub cleanup: CleanupConfig,
    pub tmux: TmuxConfig,
    pub logging: LoggingConfig,
    pub tune: TuneConfig,
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
    /// Cycle scope: `any` (default), `active` (alias `current`), `next`,
    /// `active-or-next`, or `none` (issues without a cycle). Applied server-side.
    pub cycle: String,
    /// Only issues in one of these projects (matched by project *name*,
    /// server-side). Empty = any project.
    pub projects: Vec<String>,
    /// State to move an issue to when a session starts. Empty = no change.
    pub in_progress_state: Option<String>,
    /// State to move an issue to when the task completes. Empty = no change.
    pub done_state: Option<String>,
    /// State to move an issue to when the task fails permanently or is blocked.
    pub blocked_state: Option<String>,
    /// State to move a parent issue (one with sub-issues) to once every
    /// sub-issue is completed or canceled. Empty = no change.
    pub done_state_parent: Option<String>,
    /// Let powerqueue move issues between workflow states at all. `false`
    /// when your own Claude skills or CI own the status; comments still
    /// follow `post_comments`.
    pub manage_states: bool,
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
            cycle: "any".to_string(),
            projects: Vec::new(),
            in_progress_state: Some("In Progress".to_string()),
            done_state: Some("In Review".to_string()),
            blocked_state: None,
            done_state_parent: Some("Done".to_string()),
            manage_states: true,
            post_comments: true,
            poll_interval_secs: 60,
            max_issues: 100,
            endpoint: "https://api.linear.app/graphql".to_string(),
        }
    }
}

/// Which cycles `[linear]` pulls issues from (`linear.cycle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CycleScope {
    /// Every issue regardless of cycle.
    #[default]
    Any,
    /// Only the team's active cycle.
    Active,
    /// Only the upcoming cycle.
    Next,
    /// The active cycle or the upcoming one.
    ActiveOrNext,
    /// Only issues that are in no cycle.
    None,
}

impl CycleScope {
    /// Accepted spellings of `linear.cycle` (for messages).
    pub const NAMES: [&'static str; 6] = ["any", "active", "current", "next", "active-or-next", "none"];

    /// Parse a config value (case-insensitive; `current` is an alias of `active`).
    /// `None` for an unknown word.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "" | "any" | "all" => Self::Any,
            "active" | "current" => Self::Active,
            "next" | "upcoming" => Self::Next,
            "active-or-next" | "current-or-next" => Self::ActiveOrNext,
            "none" | "null" => Self::None,
            _ => return None,
        })
    }

    /// Canonical spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Active => "active",
            Self::Next => "next",
            Self::ActiveOrNext => "active-or-next",
            Self::None => "none",
        }
    }
}

impl std::fmt::Display for CycleScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl LinearConfig {
    /// The parsed cycle scope; an unknown value (reported by
    /// [`Config::validate`]) reads as `any` so sync never silently narrows.
    pub fn cycle_scope(&self) -> CycleScope {
        CycleScope::parse(&self.cycle).unwrap_or_default()
    }

    /// Project names to filter on, trimmed, empty entries dropped.
    pub fn project_names(&self) -> Vec<String> {
        self.projects.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
    }
}

/// How the first message of a session is composed (`[prompt]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    /// Path to a Markdown template with `{{placeholders}}` (see README,
    /// "Prompt template"). `~` is expanded; a relative path is resolved
    /// against the config directory (or the repository root when it comes
    /// from `.powerqueue.toml`). `None` = the built-in prompt.
    pub template: Option<String>,
    /// Extra instructions appended to every prompt under `## Instructions`
    /// (also available to templates as `{{instructions}}`).
    pub instructions: Option<String>,
    /// Directory relative `template` paths resolve against; set by
    /// [`Config::load`] to the config directory. Not part of the file.
    #[serde(skip)]
    pub base_dir: Option<PathBuf>,
}

impl PromptConfig {
    /// Absolute path of the template, if one is configured. `~` is expanded
    /// and a relative path is joined to `base_dir` (left as-is without one).
    pub fn template_path(&self) -> Option<PathBuf> {
        let raw = self.template.as_deref().map(str::trim).filter(|t| !t.is_empty())?;
        let path = expand_tilde(raw);
        Some(match (&self.base_dir, path.is_absolute()) {
            (Some(base), false) => base.join(path),
            _ => path,
        })
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

/// Default `scheduler.review_prompt`: the `/ship-pr` skill picks the PR up
/// again with the reason the watcher found.
pub const DEFAULT_REVIEW_PROMPT: &str = "/ship-pr {pr} --reason {reason} {detail}";

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
    /// How often the PR of each `in_review` task is checked with `gh`; 0
    /// stops the watcher (tasks then stay `in_review` until handled by hand).
    pub pr_poll_secs: u64,
    /// Relaunches of one task for its PR (conflict, failed check, review)
    /// before it goes to `needs_attention`. Independent of `max_attempts`.
    pub review_rounds_max: u32,
    /// A PR whose status has not changed for this long is reported in a
    /// Linear comment and the task goes to `needs_attention`; 0 disables.
    pub review_stale_hours: u64,
    /// Label of a PR that a human merges by hand: while the PR is blocked
    /// and carries it, the watcher waits (no stale report).
    pub merge_hold_label: String,
    /// Prompt of a resumed review session. Placeholders: `{pr}` (number),
    /// `{url}`, `{reason}` (`conflict`, `ci_failed`, `review`), `{detail}`.
    pub review_prompt: String,
    /// GitHub CLI used by the PR watcher.
    pub gh_binary: String,
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
            pr_poll_secs: 120,
            review_rounds_max: 5,
            review_stale_hours: 24,
            merge_hold_label: "merge/hold".to_string(),
            review_prompt: DEFAULT_REVIEW_PROMPT.to_string(),
            gh_binary: "gh".to_string(),
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

/// Permission modes Claude Code accepts for `--permission-mode`.
pub const CLAUDE_PERMISSION_MODES: [&str; 7] =
    ["default", "manual", "acceptEdits", "plan", "auto", "dontAsk", "bypassPermissions"];

/// Approval modes for Codex sessions (`codex.approval`).
pub const CODEX_APPROVAL_MODES: [&str; 4] = ["workspace-write", "approve-for-me", "yolo", "on-request"];

/// Permission modes for Antigravity sessions (`gemini.mode`).
pub const GEMINI_MODES: [&str; 3] = ["skip-permissions", "accept-edits", "plan"];

/// How OpenAI Codex CLI is launched (see `session::codex`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodexConfig {
    pub binary: String,
    /// `workspace-write` (`-a never -s workspace-write`), `approve-for-me`,
    /// `yolo` (`--dangerously-bypass-approvals-and-sandbox`) or `on-request`.
    pub approval: String,
    /// `-c model_reasoning_effort=<value>` when set (`low|medium|high|xhigh|max|ultra`).
    pub reasoning_effort: Option<String>,
    /// Extra flags appended verbatim.
    pub extra_args: Vec<String>,
    /// Environment variables for the session.
    pub env: BTreeMap<String, String>,
    /// Pass `-c 'projects."<worktree>".trust_level="trusted"'` so Codex does
    /// not stop at its trust prompt.
    pub trust_workspace: bool,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            binary: "codex".to_string(),
            approval: "workspace-write".to_string(),
            reasoning_effort: Some("high".to_string()),
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            trust_workspace: true,
        }
    }
}

/// How Google's Antigravity CLI (`agy`) is launched (see `session::gemini`).
/// Experimental: the CLI shapes are unverified community reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeminiConfig {
    pub binary: String,
    /// `skip-permissions` (`--dangerously-skip-permissions`), `accept-edits` or `plan` (`--mode`).
    pub mode: String,
    /// `--effort` value when set (`low|medium|high`).
    pub effort: Option<String>,
    /// Extra flags appended verbatim.
    pub extra_args: Vec<String>,
    /// Environment variables for the session.
    pub env: BTreeMap<String, String>,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            binary: "agy".to_string(),
            mode: "skip-permissions".to_string(),
            effort: Some("high".to_string()),
            extra_args: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

/// Provider-neutral view of the launch settings of one provider, for code
/// that must not care which `[claude]` / `[codex]` / `[gemini]` table it
/// reads (`doctor`, the launcher's generic parts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSettings {
    pub provider: Provider,
    pub binary: String,
    /// `claude.permission_mode`, `codex.approval` or `gemini.mode`.
    pub mode: String,
    /// `claude.effort`, `codex.reasoning_effort` or `gemini.effort`.
    pub effort: Option<String>,
    pub extra_args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub trust_workspace: bool,
}

/// Model-usage pacing: shared knobs plus one budget per provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    /// One budget per provider; disabled providers are never scheduled.
    pub providers: ProviderBudgets,
    /// Fallback order between enabled providers. Enabled providers missing
    /// from the list come after it, in `claude, codex, gemini` order.
    pub provider_order: Vec<Provider>,
    /// Default model when nothing else applies.
    pub default_model: ModelTier,
    /// Model used for `low` tasks.
    pub low_model: ModelTier,
    /// Keep this fraction of the period budget unspent as a safety margin.
    pub safety_margin: f64,
    /// Fraction of the period after which unused reserved capacity is released ("end game").
    pub endgame_fraction: f64,
    /// How often the daemon runs usage probes, in minutes (0 disables them).
    pub probe_interval_mins: u64,
    /// Legacy keys moved by [`migrate_legacy_budget`] when the file was
    /// loaded (`budget.period_hours -> budget.providers.claude.period_hours`).
    /// Not part of the file.
    #[serde(skip)]
    pub migrated_keys: Vec<String>,
    /// Legacy keys that clashed with an explicit new value; reported as
    /// problems by [`Config::validate`]. Not part of the file.
    #[serde(skip)]
    pub migration_conflicts: Vec<String>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            providers: ProviderBudgets::default(),
            provider_order: Provider::ALL.to_vec(),
            default_model: ModelTier::sonnet(),
            low_model: ModelTier::sonnet(),
            safety_margin: 0.05,
            endgame_fraction: 0.8,
            probe_interval_mins: 15,
            migrated_keys: Vec::new(),
            migration_conflicts: Vec::new(),
        }
    }
}

impl BudgetConfig {
    /// The budget of one provider (every provider always has one).
    pub fn provider(&self, p: Provider) -> &ProviderBudget {
        self.providers.get(p)
    }

    /// The policy of a model, looked up in its provider's table.
    pub fn model_budget(&self, model: &ModelTier) -> Option<&ModelBudget> {
        self.providers.get(model.provider()).models.get(model)
    }

    /// Enabled providers in `provider_order`, followed by enabled providers
    /// the order does not mention (in `Provider::ALL` order).
    pub fn enabled_providers_in_order(&self) -> Vec<Provider> {
        let mut out: Vec<Provider> = Vec::new();
        for p in self.provider_order.iter().chain(Provider::ALL.iter()) {
            if self.providers.get(*p).enabled && !out.contains(p) {
                out.push(*p);
            }
        }
        out
    }

    /// Every configured model of `provider`, most capable first (ascending
    /// `rank`, then name). Includes disabled models; filter on `enabled`.
    pub fn models_for(&self, provider: Provider) -> Vec<ModelTier> {
        let mut models: Vec<(&ModelTier, &ModelBudget)> = self.providers.get(provider).models.iter().collect();
        models.sort_by(|(ma, a), (mb, b)| a.rank.cmp(&b.rank).then_with(|| ma.cmp(mb)));
        models.into_iter().map(|(m, _)| m.clone()).collect()
    }

    /// The next less capable model of the same provider (next higher rank),
    /// if any. `None` for models that are not in the config.
    pub fn downgrade(&self, model: &ModelTier) -> Option<ModelTier> {
        let ranked = self.models_for(model.provider());
        let pos = ranked.iter().position(|m| m == model)?;
        ranked.get(pos + 1).cloned()
    }

    /// `model` followed by its successive downgrades, most capable first.
    /// A model that is not in the config is the whole chain by itself.
    pub fn downgrade_chain(&self, model: &ModelTier) -> Vec<ModelTier> {
        let ranked = self.models_for(model.provider());
        match ranked.iter().position(|m| m == model) {
            Some(pos) => ranked[pos..].to_vec(),
            None => vec![model.clone()],
        }
    }
}

/// One [`ProviderBudget`] per provider. Partial tables in `config.toml` keep
/// the provider's shipped defaults for every key they omit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "RawProviderBudgets")]
pub struct ProviderBudgets {
    pub claude: ProviderBudget,
    pub codex: ProviderBudget,
    pub gemini: ProviderBudget,
}

impl Default for ProviderBudgets {
    fn default() -> Self {
        Self {
            claude: ProviderBudget::defaults_for(Provider::Claude),
            codex: ProviderBudget::defaults_for(Provider::Codex),
            gemini: ProviderBudget::defaults_for(Provider::Gemini),
        }
    }
}

impl ProviderBudgets {
    pub fn get(&self, p: Provider) -> &ProviderBudget {
        match p {
            Provider::Claude => &self.claude,
            Provider::Codex => &self.codex,
            Provider::Gemini => &self.gemini,
        }
    }

    pub fn get_mut(&mut self, p: Provider) -> &mut ProviderBudget {
        match p {
            Provider::Claude => &mut self.claude,
            Provider::Codex => &mut self.codex,
            Provider::Gemini => &mut self.gemini,
        }
    }

    /// Every provider with its budget, in `Provider::ALL` order.
    pub fn iter(&self) -> impl Iterator<Item = (Provider, &ProviderBudget)> {
        Provider::ALL.iter().map(move |p| (*p, self.get(*p)))
    }

    /// Providers with `enabled = true`, in `Provider::ALL` order.
    pub fn enabled(&self) -> Vec<Provider> {
        self.iter().filter(|(_, b)| b.enabled).map(|(p, _)| p).collect()
    }
}

/// Wire format of `[budget.providers]`: every key optional so omitted ones
/// fall back to the provider's defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawProviderBudgets {
    claude: RawProviderBudget,
    codex: RawProviderBudget,
    gemini: RawProviderBudget,
}

impl From<RawProviderBudgets> for ProviderBudgets {
    fn from(raw: RawProviderBudgets) -> Self {
        Self {
            claude: raw.claude.apply_to(ProviderBudget::defaults_for(Provider::Claude)),
            codex: raw.codex.apply_to(ProviderBudget::defaults_for(Provider::Codex)),
            gemini: raw.gemini.apply_to(ProviderBudget::defaults_for(Provider::Gemini)),
        }
    }
}

/// Subscription budget of one provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderBudget {
    /// Schedule tasks on this provider.
    pub enabled: bool,
    /// Length of the usage period in hours (subscriptions reset weekly).
    pub period_hours: u64,
    /// RFC 3339 instant at which a period starts; later periods repeat every
    /// `period_hours`. Set with `powerqueue budget set-reset <time>` or
    /// learned by a usage probe.
    pub period_anchor: Option<String>,
    /// Rolling short window (hours) that also limits usage; 0 = no window.
    pub window_hours: u64,
    /// Total weighted tokens the period may consume across all models.
    pub period_weighted_tokens: u64,
    /// Weighted tokens the rolling window may consume.
    pub window_weighted_tokens: u64,
    /// Pause scheduling on this provider when a `rate_limit` error is
    /// reported, for this many minutes (unless the reset comes sooner).
    pub rate_limit_cooldown_mins: u64,
    /// Per-model policy, keyed by model alias.
    pub models: BTreeMap<ModelTier, ModelBudget>,
}

impl ProviderBudget {
    /// The shipped budget of a provider: Claude as before (fable/opus/sonnet/
    /// haiku), Codex with the GPT-6 family, Gemini with gemini-3-pro/flash.
    /// Only Claude is enabled by default.
    pub fn defaults_for(provider: Provider) -> ProviderBudget {
        let model = |rank: u32, share: f64, min: Criticality, relax: f64, weight: f64| ModelBudget {
            rank,
            share,
            min_criticality: min,
            relax_after_fraction: relax,
            weight,
            enabled: true,
        };
        let mut models = BTreeMap::new();
        match provider {
            Provider::Claude => {
                models.insert(ModelTier::fable(), model(10, 0.25, Criticality::Critical, 0.5, 5.0));
                models.insert(ModelTier::opus(), model(20, 0.35, Criticality::High, 0.3, 3.0));
                models.insert(ModelTier::sonnet(), model(30, 0.35, Criticality::Low, 0.0, 1.0));
                models.insert(ModelTier::haiku(), model(40, 0.05, Criticality::Low, 0.0, 0.2));
                ProviderBudget {
                    enabled: true,
                    period_hours: 24 * 7,
                    period_anchor: None,
                    window_hours: 5,
                    period_weighted_tokens: 80_000_000,
                    window_weighted_tokens: 12_000_000,
                    rate_limit_cooldown_mins: 30,
                    models,
                }
            }
            Provider::Codex => {
                models.insert(ModelTier::new("gpt-6.1-sol"), model(10, 0.4, Criticality::Critical, 0.5, 2.0));
                models.insert(ModelTier::new("gpt-6-astra"), model(20, 0.4, Criticality::High, 0.3, 1.5));
                models.insert(ModelTier::new("gpt-6-luna"), model(30, 0.2, Criticality::Low, 0.0, 0.5));
                ProviderBudget {
                    enabled: false,
                    period_hours: 24 * 7,
                    period_anchor: None,
                    window_hours: 5,
                    period_weighted_tokens: 60_000_000,
                    window_weighted_tokens: 8_000_000,
                    rate_limit_cooldown_mins: 30,
                    models,
                }
            }
            Provider::Gemini => {
                models.insert(ModelTier::new("gemini-3-pro"), model(10, 0.7, Criticality::High, 0.3, 1.0));
                models.insert(ModelTier::new("gemini-3-flash"), model(20, 0.3, Criticality::Low, 0.0, 0.2));
                ProviderBudget {
                    enabled: false,
                    period_hours: 24 * 7,
                    period_anchor: None,
                    window_hours: 5,
                    period_weighted_tokens: 40_000_000,
                    window_weighted_tokens: 6_000_000,
                    rate_limit_cooldown_mins: 30,
                    models,
                }
            }
        }
    }

    /// Enabled models with a positive share, most capable first.
    pub fn enabled_models(&self) -> Vec<ModelTier> {
        let mut models: Vec<(&ModelTier, &ModelBudget)> =
            self.models.iter().filter(|(_, m)| m.enabled && m.share > 0.0).collect();
        models.sort_by(|(ma, a), (mb, b)| a.rank.cmp(&b.rank).then_with(|| ma.cmp(mb)));
        models.into_iter().map(|(m, _)| m.clone()).collect()
    }
}

/// Wire format of one provider table: every key optional. Model tables are
/// merged into the shipped ones (a model you mention keeps the shipped
/// `rank`/`weight`/... for keys you omit; models you do not mention stay).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawProviderBudget {
    enabled: Option<bool>,
    period_hours: Option<u64>,
    period_anchor: Option<String>,
    window_hours: Option<u64>,
    period_weighted_tokens: Option<u64>,
    window_weighted_tokens: Option<u64>,
    rate_limit_cooldown_mins: Option<u64>,
    models: BTreeMap<ModelTier, RawModelBudget>,
}

impl RawProviderBudget {
    fn apply_to(self, mut base: ProviderBudget) -> ProviderBudget {
        if let Some(v) = self.enabled {
            base.enabled = v;
        }
        if let Some(v) = self.period_hours {
            base.period_hours = v;
        }
        if self.period_anchor.is_some() {
            base.period_anchor = self.period_anchor;
        }
        if let Some(v) = self.window_hours {
            base.window_hours = v;
        }
        if let Some(v) = self.period_weighted_tokens {
            base.period_weighted_tokens = v;
        }
        if let Some(v) = self.window_weighted_tokens {
            base.window_weighted_tokens = v;
        }
        if let Some(v) = self.rate_limit_cooldown_mins {
            base.rate_limit_cooldown_mins = v;
        }
        for (model, raw) in self.models {
            let shipped = base.models.get(&model).copied().unwrap_or_default();
            base.models.insert(model, raw.apply_to(shipped));
        }
        base
    }
}

/// Policy for one model.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelBudget {
    /// Capability order within the provider: lower is more capable (shipped
    /// models use 10/20/30/40; user additions default to 50).
    pub rank: u32,
    /// Fraction of `period_weighted_tokens` this model may use.
    pub share: f64,
    /// Tasks must be at least this critical to get this model early in the period.
    pub min_criticality: Criticality,
    /// After this fraction of the period has elapsed, if the model is under-spent,
    /// tasks one level below `min_criticality` may use it as well (and two levels
    /// below once in the end game).
    pub relax_after_fraction: f64,
    /// Cost weight relative to Sonnet = 1.0.
    pub weight: f64,
    pub enabled: bool,
}

impl Default for ModelBudget {
    fn default() -> Self {
        Self { rank: 50, share: 0.25, min_criticality: Criticality::Low, relax_after_fraction: 0.0, weight: 1.0, enabled: true }
    }
}

/// Wire format of one model table: every key optional.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawModelBudget {
    rank: Option<u32>,
    share: Option<f64>,
    min_criticality: Option<Criticality>,
    relax_after_fraction: Option<f64>,
    weight: Option<f64>,
    enabled: Option<bool>,
}

impl RawModelBudget {
    fn apply_to(self, mut base: ModelBudget) -> ModelBudget {
        if let Some(v) = self.rank {
            base.rank = v;
        }
        if let Some(v) = self.share {
            base.share = v;
        }
        if let Some(v) = self.min_criticality {
            base.min_criticality = v;
        }
        if let Some(v) = self.relax_after_fraction {
            base.relax_after_fraction = v;
        }
        if let Some(v) = self.weight {
            base.weight = v;
        }
        if let Some(v) = self.enabled {
            base.enabled = v;
        }
        base
    }
}

/// Flat `[budget]` keys of older versions that now live under
/// `budget.providers.claude`.
pub const LEGACY_BUDGET_KEYS: [&str; 6] = [
    "period_hours",
    "period_anchor",
    "window_hours",
    "period_weighted_tokens",
    "window_weighted_tokens",
    "rate_limit_cooldown_mins",
];

/// The new dotted path of a legacy budget key (`budget.period_hours` →
/// `budget.providers.claude.period_hours`, `budget.models.fable.share` →
/// `budget.providers.claude.models.fable.share`); `None` for keys that did
/// not move.
pub fn rewrite_legacy_key(key: &str) -> Option<String> {
    let rest = key.strip_prefix("budget.")?;
    let head = rest.split('.').next().unwrap_or(rest);
    if head == "models" || LEGACY_BUDGET_KEYS.contains(&head) { Some(format!("budget.providers.claude.{rest}")) } else { None }
}

/// Move the flat `[budget]` keys and `[budget.models.*]` tables of older
/// versions under `budget.providers.claude`, in place. Returns one note per
/// moved key (`budget.period_hours -> budget.providers.claude.period_hours`).
///
/// A legacy key whose new location already holds the *same* value is simply
/// dropped (noted as moved); one whose new location holds a different value
/// is left out of the table and reported with a `conflict:` prefix so
/// [`Config::validate`] can refuse the file. Never fails; a `budget` entry
/// that is not a table is left alone for the deserializer to reject.
pub fn migrate_legacy_budget(table: &mut toml::Table) -> Vec<String> {
    let mut notes = Vec::new();
    let Some(budget) = table.get_mut("budget").and_then(|b| b.as_table_mut()) else { return notes };
    let mut moved: Vec<(String, toml::Value)> = Vec::new();
    for key in LEGACY_BUDGET_KEYS {
        if let Some(v) = budget.remove(key) {
            moved.push((key.to_string(), v));
        }
    }
    if let Some(models) = budget.remove("models") {
        moved.push(("models".to_string(), models));
    }
    if moved.is_empty() {
        return notes;
    }
    let providers = budget.entry("providers").or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let Some(providers) = providers.as_table_mut() else {
        for (key, _) in moved {
            notes.push(format!("conflict: budget.{key} cannot move because budget.providers is not a table"));
        }
        return notes;
    };
    let claude = providers.entry("claude").or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let Some(claude) = claude.as_table_mut() else {
        for (key, _) in moved {
            notes.push(format!("conflict: budget.{key} cannot move because budget.providers.claude is not a table"));
        }
        return notes;
    };
    for (key, value) in moved {
        match claude.get(&key) {
            None => {
                claude.insert(key.clone(), value);
                notes.push(format!("budget.{key} -> budget.providers.claude.{key}"));
            }
            Some(existing) if *existing == value => {
                notes.push(format!("budget.{key} -> budget.providers.claude.{key} (same value; remove the old key)"));
            }
            Some(existing) => {
                notes.push(format!(
                    "conflict: budget.{key} = {} differs from budget.providers.claude.{key} = {}; remove the old key",
                    value, existing
                ));
            }
        }
    }
    notes
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
    /// Commit changes the agent left uncommitted before pushing or removing
    /// the worktree (`powerqueue: uncommitted changes from <key>`), so a
    /// sandboxed or interrupted session never loses work.
    pub commit_uncommitted: bool,
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
            commit_uncommitted: true,
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

/// `powerqueue tune`: a headless Claude Code session that edits drafts of
/// `PRIORITY.md` and `config.toml` from a plain-language request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuneConfig {
    /// Claude model alias for the tuning session (`sonnet`, `opus`, `fable`, `haiku`).
    pub model: String,
    /// Kill the session after this many seconds (the draft is kept).
    pub timeout_secs: u64,
    /// Extra flags appended to the `claude -p` command line.
    pub extra_args: Vec<String>,
    /// Keep at most this many drafts under `<state>/tune/`; older applied,
    /// failed or unchanged drafts are pruned after each run (proposed drafts
    /// are never pruned).
    pub keep_drafts: usize,
}

impl Default for TuneConfig {
    fn default() -> Self {
        Self { model: "sonnet".to_string(), timeout_secs: 600, extra_args: Vec::new(), keep_drafts: 20 }
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
    pub codex: Option<CodexOverrides>,
    pub gemini: Option<GeminiOverrides>,
    /// Extra instructions appended to every task prompt for this repo.
    pub instructions: Option<String>,
    /// Prompt template for this repo (overrides `[prompt] template`); a
    /// relative path is resolved against the repository root.
    pub prompt_template: Option<String>,
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

/// `[codex]` subset a repository may override.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CodexOverrides {
    pub approval: Option<String>,
    pub reasoning_effort: Option<String>,
    pub extra_args: Option<Vec<String>>,
}

/// `[gemini]` subset a repository may override.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct GeminiOverrides {
    pub mode: Option<String>,
    pub effort: Option<String>,
    pub extra_args: Option<Vec<String>>,
}

impl Config {
    /// Load `config.toml` from the resolved paths. A missing file is an error
    /// with a hint to run `powerqueue init`. Legacy `[budget]` keys are moved
    /// under `budget.providers.claude` (see [`Config::deprecations`]).
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("cannot read {} (run `powerqueue init` first)", file.display()))?;
        let mut cfg = Self::from_toml(&text).with_context(|| format!("invalid config {}", file.display()))?;
        cfg.prompt.base_dir = Some(paths.config_dir.clone());
        Ok(cfg)
    }

    /// Load a draft `config.toml` from `file` as if it were the live one:
    /// same parsing and legacy-key migration as [`Config::load`], the prompt
    /// base directory set to the live config directory, and the repository's
    /// `.powerqueue.toml` overrides applied when the repo exists. Fails when
    /// the file cannot be read or parsed; [`Config::validate`] is *not* run.
    pub fn load_draft(paths: &Paths, file: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
        let mut cfg = Self::from_toml(&text).with_context(|| format!("invalid config {}", file.display()))?;
        cfg.prompt.base_dir = Some(paths.config_dir.clone());
        let repo = cfg.repo_path();
        if !cfg.repo.path.trim().is_empty() && repo.exists() {
            cfg.apply_repo_overrides(&repo)?;
        }
        Ok(cfg)
    }

    /// Load if present, otherwise defaults (for commands that must work before init).
    pub fn load_or_default(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        if file.exists() { Self::load(paths) } else { Ok(Self::default()) }
    }

    /// Parse TOML text. Flat legacy budget keys are migrated first
    /// ([`migrate_legacy_budget`]); the notes land in `budget.migrated_keys`
    /// and conflicts in `budget.migration_conflicts`. Fails on syntax errors,
    /// unknown keys and type mismatches.
    pub fn from_toml(text: &str) -> Result<Self> {
        let mut table: toml::Table = text.parse::<toml::Table>()?;
        let notes = migrate_legacy_budget(&mut table);
        let mut cfg: Config = table.try_into()?;
        let (conflicts, moved): (Vec<String>, Vec<String>) = notes.into_iter().partition(|n| n.starts_with("conflict: "));
        cfg.budget.migrated_keys = moved;
        cfg.budget.migration_conflicts = conflicts.into_iter().map(|c| c.trim_start_matches("conflict: ").to_string()).collect();
        cfg.normalise();
        Ok(cfg)
    }

    /// Post-load clean-up: an empty `linear.in_progress_state` /
    /// `done_state` / `blocked_state` / `done_state_parent` means "no state change for that
    /// transition" and becomes `None`; an empty `prompt.template` /
    /// `prompt.instructions` becomes `None` too.
    pub fn normalise(&mut self) {
        let blank = |s: &mut Option<String>| {
            if s.as_deref().is_some_and(|v| v.trim().is_empty()) {
                *s = None;
            }
        };
        blank(&mut self.linear.in_progress_state);
        blank(&mut self.linear.done_state);
        blank(&mut self.linear.blocked_state);
        blank(&mut self.linear.done_state_parent);
        blank(&mut self.prompt.template);
        blank(&mut self.prompt.instructions);
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
        let mut ov: RepoOverrides = toml::from_str(&text).with_context(|| format!("invalid {}", file.display()))?;
        if let Some(t) = ov.prompt_template.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            let path = expand_tilde(t);
            let resolved = if path.is_absolute() { path } else { repo_path.join(path) };
            ov.prompt_template = Some(resolved.to_string_lossy().to_string());
        }
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
        if let Some(c) = ov.codex {
            if let Some(v) = c.approval {
                self.codex.approval = v;
            }
            if c.reasoning_effort.is_some() {
                self.codex.reasoning_effort = c.reasoning_effort;
            }
            if let Some(v) = c.extra_args {
                self.codex.extra_args = v;
            }
        }
        if let Some(g) = ov.gemini {
            if let Some(v) = g.mode {
                self.gemini.mode = v;
            }
            if g.effort.is_some() {
                self.gemini.effort = g.effort;
            }
            if let Some(v) = g.extra_args {
                self.gemini.extra_args = v;
            }
        }
        if let Some(instr) = ov.instructions {
            self.claude.append_system_prompt = Some(match &self.claude.append_system_prompt {
                Some(existing) => format!("{existing}\n\n{instr}"),
                None => instr,
            });
        }
        if let Some(t) = ov.prompt_template.filter(|t| !t.trim().is_empty()) {
            self.prompt.template = Some(t);
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

    /// The launch settings of one provider in a provider-neutral shape.
    pub fn launch_settings(&self, provider: Provider) -> LaunchSettings {
        match provider {
            Provider::Claude => LaunchSettings {
                provider,
                binary: self.claude.binary.clone(),
                mode: self.claude.permission_mode.clone(),
                effort: self.claude.effort.clone(),
                extra_args: self.claude.extra_args.clone(),
                env: self.claude.env.clone(),
                trust_workspace: self.claude.trust_workspace,
            },
            Provider::Codex => LaunchSettings {
                provider,
                binary: self.codex.binary.clone(),
                mode: self.codex.approval.clone(),
                effort: self.codex.reasoning_effort.clone(),
                extra_args: self.codex.extra_args.clone(),
                env: self.codex.env.clone(),
                trust_workspace: self.codex.trust_workspace,
            },
            Provider::Gemini => LaunchSettings {
                provider,
                binary: self.gemini.binary.clone(),
                mode: self.gemini.mode.clone(),
                effort: self.gemini.effort.clone(),
                extra_args: self.gemini.extra_args.clone(),
                env: self.gemini.env.clone(),
                trust_workspace: false,
            },
        }
    }

    /// Deprecation notes: legacy `[budget]` keys that were moved on load.
    /// Empty for files in the current shape. Not problems; `validate` only
    /// reports legacy keys that conflict with explicit new values.
    pub fn deprecations(&self) -> Vec<String> {
        self.budget
            .migrated_keys
            .iter()
            .map(|k| format!("{k}: move it in config.toml (old flat budget keys now live under [budget.providers.claude])"))
            .collect()
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
        if self.scheduler.review_prompt.trim().is_empty() {
            problems.push("scheduler.review_prompt is empty (it is what a resumed review session is told)".to_string());
        }
        if self.scheduler.gh_binary.trim().is_empty() {
            problems.push("scheduler.gh_binary is empty (the PR watcher runs it)".to_string());
        }
        for conflict in &self.budget.migration_conflicts {
            problems.push(format!("legacy budget key {conflict}"));
        }
        match self.tune.model.parse::<ModelTier>() {
            Ok(m) if m.provider() != Provider::Claude => {
                problems.push(format!("tune.model `{}` is not a Claude model (tune runs on Claude Code)", self.tune.model))
            }
            Ok(_) => {}
            Err(e) => problems.push(format!("tune.model: {e}")),
        }
        if self.tune.timeout_secs == 0 {
            problems.push("tune.timeout_secs must be >= 1".to_string());
        }
        if self.tune.keep_drafts == 0 {
            problems.push("tune.keep_drafts must be >= 1".to_string());
        }
        let b = &self.budget;
        for (p, pb) in b.providers.iter() {
            let prefix = format!("budget.providers.{p}");
            if pb.period_hours == 0 {
                problems.push(format!("{prefix}.period_hours must be >= 1"));
            }
            if pb.window_hours > pb.period_hours {
                problems.push(format!("{prefix}.window_hours cannot exceed {prefix}.period_hours"));
            }
            let share: f64 = pb.models.values().filter(|m| m.enabled).map(|m| m.share).sum();
            if share > 1.0 + 1e-6 {
                problems.push(format!("{prefix}.models shares sum to {share:.2} (> 1.0)"));
            }
            if let Some(anchor) = &pb.period_anchor
                && chrono::DateTime::parse_from_rfc3339(anchor).is_err()
            {
                problems.push(format!("{prefix}.period_anchor `{anchor}` is not RFC 3339"));
            }
            if pb.enabled && !pb.models.values().any(|m| m.enabled && m.share > 0.0) {
                problems.push(format!("{prefix} is enabled but has no enabled model with a share > 0"));
            }
            for (model, m) in &pb.models {
                let key = format!("{prefix}.models.{model}");
                match model.known_provider() {
                    None => problems.push(format!(
                        "{key}: `{model}` is not a known model name; use a known alias or the explicit `{p}:{model}` form"
                    )),
                    Some(owner) if owner != p => problems
                        .push(format!("{key}: `{model}` belongs to {owner}, not {p}; move it to budget.providers.{owner}")),
                    Some(_) => {}
                }
                if !(0.0..=1.0).contains(&m.share) {
                    problems.push(format!("{key}.share must be between 0 and 1"));
                }
                if !(0.0..=1.0).contains(&m.relax_after_fraction) {
                    problems.push(format!("{key}.relax_after_fraction must be between 0 and 1"));
                }
                if m.weight <= 0.0 {
                    problems.push(format!("{key}.weight must be > 0"));
                }
            }
        }
        if !(0.0..=0.5).contains(&b.safety_margin) {
            problems.push("budget.safety_margin must be between 0 and 0.5".to_string());
        }
        if !(0.0..=1.0).contains(&b.endgame_fraction) {
            problems.push("budget.endgame_fraction must be between 0 and 1".to_string());
        }
        let mut seen = Vec::new();
        for p in &b.provider_order {
            if seen.contains(p) {
                problems.push(format!("budget.provider_order lists `{p}` more than once"));
            }
            seen.push(*p);
        }
        for (name, model) in [("default_model", &b.default_model), ("low_model", &b.low_model)] {
            match model.known_provider() {
                None => problems.push(format!("budget.{name} `{model}` is not a known model name")),
                Some(p) => {
                    if !b.providers.get(p).enabled {
                        problems.push(format!("budget.{name} `{model}` belongs to {p}, which is not enabled"));
                    } else if !b.providers.get(p).models.contains_key(model) {
                        problems.push(format!("budget.{name} `{model}` is not listed in budget.providers.{p}.models"));
                    }
                }
            }
        }
        if !CLAUDE_PERMISSION_MODES.contains(&self.claude.permission_mode.as_str()) {
            problems.push(format!(
                "claude.permission_mode `{}` is not one of {}",
                self.claude.permission_mode,
                CLAUDE_PERMISSION_MODES.join("|")
            ));
        }
        if !CODEX_APPROVAL_MODES.contains(&self.codex.approval.as_str()) {
            problems.push(format!("codex.approval `{}` is not one of {}", self.codex.approval, CODEX_APPROVAL_MODES.join("|")));
        }
        if !GEMINI_MODES.contains(&self.gemini.mode.as_str()) {
            problems.push(format!("gemini.mode `{}` is not one of {}", self.gemini.mode, GEMINI_MODES.join("|")));
        }
        if self.linear.enabled && self.linear.queued_states.is_empty() {
            problems.push("linear.queued_states is empty; no issues would ever be picked up".to_string());
        }
        if CycleScope::parse(&self.linear.cycle).is_none() {
            problems.push(format!(
                "linear.cycle `{}` is not one of {} (use `any` to ignore cycles)",
                self.linear.cycle,
                CycleScope::NAMES.join("|")
            ));
        }
        if !self.linear.projects.is_empty() && self.linear.project_names().is_empty() {
            problems.push("linear.projects lists only empty names; remove the key or name the projects".to_string());
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

/// The commented template written by `powerqueue init`: every key with its
/// default, Claude enabled, the `[budget.providers.codex]` and `.gemini`
/// tables present but `enabled = false`.
pub fn config_template(repo_path: &str, team_keys: &[String]) -> String {
    let mut cfg = Config::default();
    cfg.repo.path = repo_path.to_string();
    cfg.linear.team_keys = team_keys.to_vec();
    let body = cfg.to_toml().unwrap_or_default();
    format!(
        "# powerqueue configuration\n\
         # Docs: https://github.com/aleandros/powerqueue#configuration\n\
         # Every key is optional; defaults are shown. Edit and run `powerqueue doctor`.\n\
         # Budgets are per provider: [budget.providers.claude] is live; codex and gemini\n\
         # are experimental and stay `enabled = false` until their launchers ship.\n\n{body}"
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
        assert!(cfg.deprecations().is_empty());
    }

    #[test]
    fn round_trips_through_toml() {
        let mut cfg = Config::default();
        cfg.repo.path = "/tmp/repo".into();
        cfg.linear.team_keys = vec!["ENG".into()];
        let text = cfg.to_toml().unwrap();
        assert!(text.contains("[budget.providers.claude.models.fable]"), "{text}");
        assert!(text.contains("[budget.providers.codex.models.\"gpt-6.1-sol\"]"), "{text}");
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Config::from_toml("[repo]\npath='/x'\nbogus = 1\n").unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
        let err = Config::from_toml("[budget.providers.claude]\nbogus = 1\n").unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn partial_config_uses_defaults() {
        let cfg = Config::from_toml("[repo]\npath = '/x'\n[scheduler]\nmax_concurrent = 4\n").unwrap();
        assert_eq!(cfg.scheduler.max_concurrent, 4);
        assert_eq!(cfg.scheduler.max_attempts, 3);
        assert_eq!(cfg.budget.providers.claude.models[&ModelTier::fable()].min_criticality, Criticality::Critical);
        assert!(cfg.budget.providers.claude.enabled);
        assert!(!cfg.budget.providers.codex.enabled);
        assert_eq!(cfg.budget.providers.codex.models.len(), 3);
    }

    #[test]
    fn partial_provider_tables_keep_provider_defaults() {
        let cfg = Config::from_toml(
            "[budget.providers.codex]\nenabled = true\nperiod_weighted_tokens = 1000\n\
             [budget.providers.codex.models.\"gpt-6.1-sol\"]\nshare = 0.5\n\
             [budget.providers.codex.models.\"gpt-6-nova\"]\nshare = 0.1\n\
             [budget.providers.claude.models.fable]\nenabled = false\n",
        )
        .unwrap();
        let codex = &cfg.budget.providers.codex;
        assert!(codex.enabled);
        assert_eq!(codex.period_weighted_tokens, 1000);
        assert_eq!(codex.window_weighted_tokens, 8_000_000, "omitted keys keep the codex defaults");
        assert_eq!(codex.period_hours, 168);
        let sol = codex.models[&ModelTier::new("gpt-6.1-sol")];
        assert_eq!(sol.share, 0.5);
        assert_eq!(sol.rank, 10, "shipped rank survives a partial model table");
        assert_eq!(sol.weight, 2.0);
        assert_eq!(sol.min_criticality, Criticality::Critical);
        assert_eq!(codex.models.len(), 4, "shipped models stay, the new one is added");
        let nova = codex.models[&ModelTier::new("gpt-6-nova")];
        assert_eq!(nova.rank, 50, "user additions default to rank 50");
        assert_eq!(nova.weight, 1.0);
        let fable = cfg.budget.providers.claude.models[&ModelTier::fable()];
        assert!(!fable.enabled);
        assert_eq!(fable.share, 0.25);
        assert_eq!(fable.weight, 5.0);
        assert_eq!(cfg.budget.providers.gemini, ProviderBudget::defaults_for(Provider::Gemini));
    }

    #[test]
    fn legacy_budget_keys_are_migrated_with_notes() {
        let cfg = Config::from_toml(
            "[repo]\npath = '/x'\n[budget]\nperiod_hours = 100\nperiod_anchor = '2026-09-28T00:00:00Z'\n\
             window_weighted_tokens = 5\ndefault_model = 'haiku'\nsafety_margin = 0.1\n\
             [budget.models.fable]\nshare = 0.1\nweight = 9.0\n",
        )
        .unwrap();
        let claude = &cfg.budget.providers.claude;
        assert_eq!(claude.period_hours, 100);
        assert_eq!(claude.period_anchor.as_deref(), Some("2026-09-28T00:00:00Z"));
        assert_eq!(claude.window_weighted_tokens, 5);
        assert_eq!(claude.window_hours, 5, "untouched keys keep their defaults");
        assert_eq!(claude.models[&ModelTier::fable()].share, 0.1);
        assert_eq!(claude.models[&ModelTier::fable()].weight, 9.0);
        assert_eq!(claude.models[&ModelTier::fable()].rank, 10);
        assert_eq!(claude.models.len(), 4);
        assert_eq!(cfg.budget.default_model, ModelTier::haiku(), "shared keys stay in [budget]");
        assert_eq!(cfg.budget.safety_margin, 0.1);
        assert_eq!(
            cfg.budget.migrated_keys,
            vec![
                "budget.period_hours -> budget.providers.claude.period_hours",
                "budget.period_anchor -> budget.providers.claude.period_anchor",
                "budget.window_weighted_tokens -> budget.providers.claude.window_weighted_tokens",
                "budget.models -> budget.providers.claude.models",
            ]
        );
        assert_eq!(cfg.deprecations().len(), 4);
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        // Saving writes the new shape; loading that has no notes.
        let back = Config::from_toml(&cfg.to_toml().unwrap()).unwrap();
        assert!(back.migrated_keys_empty());
        assert_eq!(back.budget.providers.claude, cfg.budget.providers.claude);
    }

    impl Config {
        fn migrated_keys_empty(&self) -> bool {
            self.budget.migrated_keys.is_empty() && self.budget.migration_conflicts.is_empty()
        }
    }

    #[test]
    fn legacy_and_new_keys_conflict_unless_equal() {
        let cfg = Config::from_toml(
            "[repo]\npath = '/x'\n[budget]\nperiod_hours = 100\nwindow_hours = 5\n[budget.providers.claude]\nperiod_hours = 168\nwindow_hours = 5\n",
        )
        .unwrap();
        assert_eq!(cfg.budget.providers.claude.period_hours, 168, "the explicit new value wins");
        assert_eq!(cfg.budget.migration_conflicts.len(), 1, "{:?}", cfg.budget.migration_conflicts);
        assert_eq!(cfg.budget.migrated_keys.len(), 1, "equal values are only a note: {:?}", cfg.budget.migrated_keys);
        let problems = cfg.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("budget.period_hours = 100 differs from budget.providers.claude.period_hours = 168")),
            "{problems:?}"
        );
    }

    #[test]
    fn migrate_legacy_budget_is_pure_and_idempotent() {
        let mut table: toml::Table = "[budget]\nperiod_hours = 10\n[budget.models.opus]\nshare = 0.2\n".parse().unwrap();
        let notes = migrate_legacy_budget(&mut table);
        assert_eq!(notes.len(), 2);
        assert_eq!(table["budget"]["providers"]["claude"]["period_hours"].as_integer(), Some(10));
        assert_eq!(table["budget"]["providers"]["claude"]["models"]["opus"]["share"].as_float(), Some(0.2));
        assert!(table["budget"].get("period_hours").is_none());
        assert!(table["budget"].get("models").is_none());
        assert!(migrate_legacy_budget(&mut table).is_empty());
        let mut none: toml::Table = "[repo]\npath = '/x'\n".parse().unwrap();
        assert!(migrate_legacy_budget(&mut none).is_empty());
    }

    #[test]
    fn rewrite_legacy_key_paths() {
        assert_eq!(
            rewrite_legacy_key("budget.models.fable.share").as_deref(),
            Some("budget.providers.claude.models.fable.share")
        );
        assert_eq!(rewrite_legacy_key("budget.period_anchor").as_deref(), Some("budget.providers.claude.period_anchor"));
        assert_eq!(rewrite_legacy_key("budget.models").as_deref(), Some("budget.providers.claude.models"));
        assert_eq!(rewrite_legacy_key("budget.default_model"), None);
        assert_eq!(rewrite_legacy_key("budget.providers.claude.period_hours"), None);
        assert_eq!(rewrite_legacy_key("claude.binary"), None);
    }

    #[test]
    fn repo_overrides_merge() {
        let mut cfg = Config::default();
        cfg.claude.append_system_prompt = Some("base".into());
        let ov: RepoOverrides = toml::from_str(
            "setup = ['npm ci']\ninstructions = 'Run tests'\n[cleanup]\nremove_worktree = false\n[claude]\npermission_mode = 'plan'\n\
             [codex]\napproval = 'yolo'\n[gemini]\nmode = 'plan'\neffort = 'low'\n",
        )
        .unwrap();
        cfg.merge_overrides(ov);
        assert_eq!(cfg.repo.setup, vec!["npm ci".to_string()]);
        assert!(!cfg.cleanup.remove_worktree);
        assert_eq!(cfg.claude.permission_mode, "plan");
        assert_eq!(cfg.claude.append_system_prompt.as_deref(), Some("base\n\nRun tests"));
        assert_eq!(cfg.codex.approval, "yolo");
        assert_eq!(cfg.gemini.mode, "plan");
        assert_eq!(cfg.gemini.effort.as_deref(), Some("low"));
    }

    #[test]
    fn launch_settings_views_each_provider() {
        let mut cfg = Config::default();
        cfg.claude.effort = Some("max".into());
        let s = cfg.launch_settings(Provider::Claude);
        assert_eq!((s.binary.as_str(), s.mode.as_str(), s.effort.as_deref()), ("claude", "acceptEdits", Some("max")));
        assert!(s.trust_workspace);
        let s = cfg.launch_settings(Provider::Codex);
        assert_eq!((s.binary.as_str(), s.mode.as_str(), s.effort.as_deref()), ("codex", "workspace-write", Some("high")));
        let s = cfg.launch_settings(Provider::Gemini);
        assert_eq!((s.binary.as_str(), s.mode.as_str()), ("agy", "skip-permissions"));
        assert!(!s.trust_workspace);
    }

    #[test]
    fn validation_catches_bad_values() {
        let mut cfg = Config::default();
        cfg.repo.path = "/x".into();
        cfg.scheduler.max_concurrent = 0;
        cfg.claude.permission_mode = "yolo".into();
        cfg.budget.providers.claude.models.get_mut(&ModelTier::fable()).unwrap().share = 0.9;
        let problems = cfg.validate();
        assert!(problems.iter().any(|p| p.contains("max_concurrent")));
        assert!(problems.iter().any(|p| p.contains("permission_mode")));
        assert!(problems.iter().any(|p| p.contains("shares sum")));
    }

    #[test]
    fn validation_covers_providers() {
        let mut cfg = Config::default();
        cfg.repo.path = "/x".into();
        cfg.budget.providers.claude.window_hours = 200;
        cfg.budget.providers.codex.models.insert(ModelTier::opus(), ModelBudget::default());
        cfg.budget.providers.claude.models.insert(ModelTier::new("turbo"), ModelBudget { share: 0.0, ..Default::default() });
        cfg.budget.provider_order = vec![Provider::Claude, Provider::Claude];
        cfg.budget.low_model = ModelTier::new("gpt-6-luna");
        cfg.codex.approval = "whatever".into();
        cfg.gemini.mode = "nope".into();
        let problems = cfg.validate();
        let has = |s: &str| problems.iter().any(|p| p.contains(s));
        assert!(has("budget.providers.claude.window_hours cannot exceed"), "{problems:?}");
        assert!(has("`opus` belongs to claude, not codex"), "{problems:?}");
        assert!(has("`turbo` is not a known model name"), "{problems:?}");
        assert!(has("provider_order lists `claude` more than once"), "{problems:?}");
        assert!(has("budget.low_model `gpt-6-luna` belongs to codex, which is not enabled"), "{problems:?}");
        assert!(has("codex.approval `whatever`"), "{problems:?}");
        assert!(has("gemini.mode `nope`"), "{problems:?}");

        let mut cfg = Config::default();
        cfg.repo.path = "/x".into();
        cfg.budget.providers.claude.window_hours = 0;
        assert!(cfg.validate().is_empty(), "window_hours = 0 means no window: {:?}", cfg.validate());
        cfg.budget.providers.codex.enabled = true;
        cfg.budget.low_model = ModelTier::new("gpt-6-luna");
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        cfg.budget.providers.claude.period_hours = 0;
        assert!(cfg.validate().iter().any(|p| p.contains("budget.providers.claude.period_hours must be >= 1")));
    }

    #[test]
    fn budget_helpers_order_by_rank() {
        let mut cfg = BudgetConfig::default();
        assert_eq!(
            cfg.models_for(Provider::Claude),
            vec![ModelTier::fable(), ModelTier::opus(), ModelTier::sonnet(), ModelTier::haiku()]
        );
        assert_eq!(cfg.downgrade(&ModelTier::fable()), Some(ModelTier::opus()));
        assert_eq!(cfg.downgrade(&ModelTier::haiku()), None);
        assert_eq!(cfg.downgrade(&ModelTier::new("gpt-6-nova")), None);
        assert_eq!(cfg.downgrade_chain(&ModelTier::opus()), vec![ModelTier::opus(), ModelTier::sonnet(), ModelTier::haiku()]);
        assert_eq!(cfg.downgrade_chain(&ModelTier::new("gpt-6-nova")), vec![ModelTier::new("gpt-6-nova")]);
        assert_eq!(cfg.downgrade(&ModelTier::new("gpt-6.1-sol")), Some(ModelTier::new("gpt-6-astra")));
        assert_eq!(cfg.enabled_providers_in_order(), vec![Provider::Claude]);
        cfg.providers.codex.enabled = true;
        cfg.providers.gemini.enabled = true;
        cfg.provider_order = vec![Provider::Gemini];
        assert_eq!(cfg.enabled_providers_in_order(), vec![Provider::Gemini, Provider::Claude, Provider::Codex]);
        assert_eq!(cfg.providers.enabled(), vec![Provider::Claude, Provider::Codex, Provider::Gemini]);
        assert_eq!(cfg.model_budget(&ModelTier::new("gemini-3-flash")).map(|m| m.rank), Some(20));
        assert!(cfg.model_budget(&ModelTier::new("codex:custom")).is_none());
        cfg.providers.claude.models.get_mut(&ModelTier::opus()).unwrap().rank = 5;
        assert_eq!(cfg.models_for(Provider::Claude)[0], ModelTier::opus(), "rank is configuration");
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
        assert!(text.contains("[budget.providers.codex]\nenabled = false"), "{text}");
        assert!(text.contains("[budget.providers.gemini]\nenabled = false"), "{text}");
        assert!(text.contains("[codex]\n"), "{text}");
        assert!(text.contains("[gemini]\n"), "{text}");
        assert!(cfg.budget.migrated_keys.is_empty());
    }

    #[test]
    fn linear_cycle_and_projects_are_validated() {
        let mut cfg = Config::default();
        cfg.repo.path = "/x".into();
        for v in ["any", "active", "Current", "next", "active-or-next", "active_or_next", "none"] {
            cfg.linear.cycle = v.into();
            assert!(cfg.validate().is_empty(), "{v}: {:?}", cfg.validate());
        }
        assert_eq!(LinearConfig { cycle: "current".into(), ..Default::default() }.cycle_scope(), CycleScope::Active);
        assert_eq!(LinearConfig { cycle: "ACTIVE-OR-NEXT".into(), ..Default::default() }.cycle_scope(), CycleScope::ActiveOrNext);
        cfg.linear.cycle = "sprint 3".into();
        let problems = cfg.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("linear.cycle `sprint 3` is not one of any|active|current|next|active-or-next|none")),
            "{problems:?}"
        );
        assert_eq!(cfg.linear.cycle_scope(), CycleScope::Any, "an invalid value never narrows the queue");
        cfg.linear.cycle = "any".into();
        cfg.linear.projects = vec![" ".into()];
        assert!(cfg.validate().iter().any(|p| p.contains("linear.projects")), "{:?}", cfg.validate());
        cfg.linear.projects = vec!["Launch".into(), " ".into()];
        assert!(cfg.validate().is_empty());
        assert_eq!(cfg.linear.project_names(), vec!["Launch".to_string()]);

        let cfg = Config::from_toml("[linear]\ncycle = 'next'\nprojects = ['A', 'B']\nmanage_states = false\n").unwrap();
        assert_eq!(cfg.linear.cycle_scope(), CycleScope::Next);
        assert_eq!(cfg.linear.projects, vec!["A".to_string(), "B".to_string()]);
        assert!(!cfg.linear.manage_states);
        assert!(Config::default().linear.manage_states);
    }

    #[test]
    fn empty_state_names_mean_no_state_change() {
        let cfg = Config::from_toml(
            "[repo]\npath = '/x'\n[linear]\nin_progress_state = ''\ndone_state = '  '\nblocked_state = 'Blocked'\ndone_state_parent = ''\n",
        )
        .unwrap();
        assert_eq!(cfg.linear.in_progress_state, None);
        assert_eq!(cfg.linear.done_state, None);
        assert_eq!(cfg.linear.blocked_state.as_deref(), Some("Blocked"));
        assert_eq!(cfg.linear.done_state_parent, None);
        assert_eq!(LinearConfig::default().done_state_parent.as_deref(), Some("Done"));
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn prompt_template_paths_resolve_against_config_dir_and_repo_root() {
        let mut cfg = Config::from_toml("[prompt]\ntemplate = 'prompt.md'\ninstructions = ''\n").unwrap();
        assert_eq!(cfg.prompt.instructions, None);
        assert_eq!(cfg.prompt.template_path(), Some(PathBuf::from("prompt.md")), "no base dir: left as-is");
        cfg.prompt.base_dir = Some(PathBuf::from("/cfg"));
        assert_eq!(cfg.prompt.template_path(), Some(PathBuf::from("/cfg/prompt.md")));
        cfg.prompt.template = Some("/abs/p.md".into());
        assert_eq!(cfg.prompt.template_path(), Some(PathBuf::from("/abs/p.md")));
        cfg.prompt.template = Some("  ".into());
        assert_eq!(cfg.prompt.template_path(), None);
        assert_eq!(Config::default().prompt.template_path(), None);

        // `Config::load` sets the base dir; `.powerqueue.toml` resolves against the repo root and wins.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            paths.config_file(),
            format!("[repo]\npath = '{}'\n[prompt]\ntemplate = 'global.md'\ninstructions = 'Be brief.'\n", repo.display()),
        )
        .unwrap();
        let mut cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.prompt.template_path(), Some(paths.config_dir.join("global.md")));
        assert_eq!(cfg.prompt.instructions.as_deref(), Some("Be brief."));
        std::fs::write(repo.join(REPO_CONFIG_FILE), "prompt_template = 'docs/agent-prompt.md'\n").unwrap();
        cfg.apply_repo_overrides(&repo).unwrap();
        assert_eq!(cfg.prompt.template_path(), Some(repo.join("docs/agent-prompt.md")));
        let text = cfg.to_toml().unwrap();
        assert!(text.contains("[prompt]"), "{text}");
        assert!(!text.contains("base_dir"), "{text}");
        assert!(Config::from_toml("[prompt]\nbogus = 1\n").is_err());
    }
}
