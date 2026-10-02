//! `powerqueue doctor` — diagnostics and tuning advice.
//!
//! Each check returns a [`CheckResult`] with a status, a one-line finding and
//! an optional fix hint. `--fix` applies the safe automatic fixes (prune
//! orphan worktrees/windows, reset stuck states, clear stale locks).
//!
//! Categories:
//! * environment: git, tmux, the CLI of every enabled provider (binary,
//!   version, logged in: `claude auth status`, `codex login status`, ...)
//! * configuration: config.toml validity, PRIORITY.md parse, repo path, worktree root writable,
//!   per provider: model shares, period anchor known (config / observed / default)
//! * secrets: keychain backend, Linear key works (`viewer`), Jev key (if enabled)
//! * state: database integrity, daemon heartbeat, orphaned worktrees / tmux windows,
//!   tasks stuck in `starting`/`running` with no live session
//! * algorithm: estimator accuracy, crash rate, idle rate, throttling frequency,
//!   per provider: probe freshness, top-model under/over-reservation, window
//!   pressure — each with a concrete suggestion

use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::budget::{Estimator, Ledger, Ledgers, load_observed, load_probe_status};
use crate::config::{Config, REPO_CONFIG_FILE, RepoOverrides};
use crate::domain::{EventLevel, ModelTier, Provider, SessionState, TaskState};
use crate::jev::JevClient;
use crate::linear::client::LinearClient;
use crate::paths::Paths;
use crate::priority::PriorityRules;
use crate::secrets::{SecretKind, Secrets};
use crate::store::Store;
use crate::tmux::Tmux;
use crate::worktree::Repo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub category: String,
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix_hint: Option<String>,
    /// Set when `--fix` repaired the problem.
    pub fixed: bool,
}

impl CheckResult {
    /// Build a result; `fixed` starts false.
    pub fn new(category: &str, name: &str, status: Status, detail: impl Into<String>) -> Self {
        Self { category: category.into(), name: name.into(), status, detail: detail.into(), fix_hint: None, fixed: false }
    }
    pub fn ok(category: &str, name: &str, detail: impl Into<String>) -> Self {
        Self::new(category, name, Status::Ok, detail)
    }
    pub fn warn(category: &str, name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(category, name, Status::Warn, detail).hint(hint)
    }
    pub fn fail(category: &str, name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(category, name, Status::Fail, detail).hint(hint)
    }
    pub fn skipped(category: &str, name: &str, detail: impl Into<String>) -> Self {
        Self::new(category, name, Status::Skipped, detail)
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        let h = hint.into();
        self.fix_hint = if h.is_empty() { None } else { Some(h) };
        self
    }
}

/// Category names, in display order.
pub const CATEGORIES: [&str; 5] = ["environment", "configuration", "secrets", "state", "algorithm"];

const ENV: &str = "environment";
const CONF: &str = "configuration";
const SEC: &str = "secrets";
const STATE: &str = "state";
const ALGO: &str = "algorithm";

// ----------------------------------------------------------------- thresholds

/// Fewer estimator samples than this and predictions are still the defaults.
pub const MIN_ESTIMATOR_SAMPLES: usize = 5;
/// Estimator MAPE above this is called inaccurate.
pub const MAX_ESTIMATOR_MAPE: f64 = 0.6;
/// Crash (and idle) rate over sessions launched that triggers a warning.
pub const MAX_CRASH_RATE: f64 = 0.3;
/// Throttle events in 24 h that trigger a warning.
pub const MAX_THROTTLES_PER_DAY: u64 = 10;
/// Window spend fraction considered "under pressure".
pub const MAX_WINDOW_FRACTION: f64 = 0.9;
/// An observation older than this many probe intervals is stale.
pub const STALE_PROBE_INTERVALS: i64 = 3;

/// Classify an event rate (`events / launches`): `Ok` when below the threshold
/// or when there were no launches, `Warn` otherwise. Returns the rate too.
pub fn rate_status(events: u64, launches: u64, threshold: f64) -> (Status, f64) {
    if launches == 0 {
        return (Status::Ok, 0.0);
    }
    let rate = events as f64 / launches as f64;
    (if rate > threshold { Status::Warn } else { Status::Ok }, rate)
}

/// Pacing advice for a provider's most capable model from the period's
/// elapsed fraction and the model's spent fraction: under-used late in the
/// period, or over-paced early. The hint names the model's own config keys.
pub fn top_model_pacing(model: &ModelTier, elapsed: f64, spent: f64) -> Option<(Status, String, String)> {
    let key = format!("budget.providers.{}.models.{}", model.provider(), model.alias());
    if elapsed > 0.7 && spent < 0.3 {
        return Some((
            Status::Warn,
            format!(
                "{model} under-used: {:.0}% of its share spent with {:.0}% of the period gone",
                spent * 100.0,
                elapsed * 100.0
            ),
            format!("lower {key}.relax_after_fraction or {key}.min_criticality so more tasks may use {model}"),
        ));
    }
    if spent > elapsed + 0.25 {
        return Some((
            Status::Warn,
            format!("{model} over-paced: {:.0}% of its share spent after {:.0}% of the period", spent * 100.0, elapsed * 100.0),
            format!("raise {key}.min_criticality or lower {key}.share"),
        ));
    }
    None
}

/// [`top_model_pacing`] for Fable (kept for callers of the Claude-only API).
pub fn fable_pacing(elapsed: f64, spent: f64) -> Option<(Status, String, String)> {
    top_model_pacing(&ModelTier::fable(), elapsed, spent)
}

/// Estimator advice from sample count and accuracy (MAPE).
pub fn estimator_status(samples: usize, mape: Option<f64>) -> (Status, String, Option<String>) {
    if samples < MIN_ESTIMATOR_SAMPLES {
        return (
            Status::Warn,
            format!(
                "{samples} completed task(s) with usage; predictions are defaults until ~{MIN_ESTIMATOR_SAMPLES} tasks complete"
            ),
            Some("let a few tasks finish; nothing to tune yet".to_string()),
        );
    }
    match mape {
        Some(m) if m > MAX_ESTIMATOR_MAPE => (
            Status::Warn,
            format!("{samples} samples, mean error {:.0}%: cost predictions are rough", m * 100.0),
            Some("add Linear estimates and consistent labels so tasks group into similar buckets".to_string()),
        ),
        Some(m) => (Status::Ok, format!("{samples} samples, mean error {:.0}%", m * 100.0), None),
        None => (Status::Ok, format!("{samples} samples (accuracy not computable yet)"), None),
    }
}

// --------------------------------------------------------------- claude auth

/// Outcome of `claude auth status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeAuth {
    pub logged_in: bool,
    pub method: Option<String>,
    pub email: Option<String>,
    /// Raw summary for messages (JSON fields or stderr).
    pub detail: String,
}

/// Interpret the output of `claude auth status`: exit 0 means logged in
/// unless the JSON says `loggedIn: false`.
pub fn parse_claude_auth(exit_ok: bool, stdout: &str, stderr: &str) -> ClaudeAuth {
    let json: Option<serde_json::Value> = serde_json::from_str(stdout.trim()).ok();
    let get = |k: &str| json.as_ref().and_then(|j| j.get(k)).and_then(|v| v.as_str()).map(str::to_string);
    let logged_in = match json.as_ref().and_then(|j| j.get("loggedIn")).and_then(|v| v.as_bool()) {
        Some(v) => v && exit_ok,
        None => exit_ok,
    };
    let method = get("authMethod");
    let email = get("email").or_else(|| get("emailAddress"));
    let detail = if logged_in {
        match (&method, &email) {
            (Some(m), Some(e)) => format!("{e} via {m}"),
            (Some(m), None) => format!("via {m}"),
            _ => "logged in".to_string(),
        }
    } else if !stderr.trim().is_empty() {
        stderr.trim().lines().next().unwrap_or_default().to_string()
    } else if !stdout.trim().is_empty() {
        stdout.trim().lines().next().unwrap_or_default().to_string()
    } else {
        "not logged in".to_string()
    };
    ClaudeAuth { logged_in, method, email, detail }
}

/// Run `claude auth status` and interpret it. Errors only when the binary
/// cannot be executed at all.
pub fn claude_auth_status(binary: &str) -> Result<ClaudeAuth> {
    let out = std::process::Command::new(binary)
        .args(["auth", "status"])
        .output()
        .with_context(|| format!("run `{binary} auth status`"))?;
    Ok(parse_claude_auth(out.status.success(), &String::from_utf8_lossy(&out.stdout), &String::from_utf8_lossy(&out.stderr)))
}

fn version_of(binary: &str, flag: &str) -> Result<String> {
    let out = std::process::Command::new(binary).arg(flag).output().with_context(|| format!("run `{binary} {flag}`"))?;
    if !out.status.success() {
        return Err(anyhow!("`{binary} {flag}` exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// -------------------------------------------------------------- environment

fn check_git() -> CheckResult {
    match version_of("git", "--version") {
        Ok(v) => CheckResult::ok(ENV, "git", v),
        Err(e) => CheckResult::fail(ENV, "git", format!("{e:#}"), "install git and make sure it is on PATH"),
    }
}

fn check_tmux(cfg: &Config) -> CheckResult {
    let tmux = Tmux::new(cfg.tmux.binary.clone(), cfg.tmux.socket_name.clone());
    match tmux.version() {
        Ok(v) => CheckResult::ok(ENV, "tmux", v),
        Err(e) => CheckResult::fail(
            ENV,
            "tmux",
            format!("{e:#}"),
            "install tmux (brew install tmux / apt install tmux) or set tmux.binary",
        ),
    }
}

/// Terminal facts for the dashboard: `TERM` and the locale charset. Pure so
/// it can be tested; `interactive` is whether stdout is a terminal (when it is
/// not, e.g. in CI, the check is skipped rather than nagging).
pub fn terminal_status(interactive: bool, term: Option<&str>, locale: Option<&str>) -> CheckResult {
    if !interactive {
        return CheckResult::skipped(
            ENV,
            "terminal",
            "stdout is not a terminal; the dashboard needs one (`dashboard --once` works anywhere)",
        );
    }
    if let Err(e) = crate::dashboard::precheck(true, true, term) {
        return CheckResult::warn(
            ENV,
            "terminal",
            e.to_string(),
            "export TERM (e.g. TERM=xterm-256color) before running `powerqueue dashboard`",
        );
    }
    let utf8 = crate::dashboard::locale_is_utf8(locale, None, None);
    let term = term.unwrap_or_default().trim().to_string();
    if !utf8 {
        return CheckResult::warn(
            ENV,
            "terminal",
            format!(
                "TERM={term}, locale `{}` is not UTF-8: the dashboard falls back to ASCII symbols",
                locale.unwrap_or_default()
            ),
            "set LANG=en_US.UTF-8 (or LC_ALL) so box-drawing glyphs render; `dashboard --ascii` forces the fallback",
        );
    }
    CheckResult::ok(ENV, "terminal", format!("TERM={term}, UTF-8 locale"))
}

fn check_terminal() -> CheckResult {
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let term = std::env::var("TERM").ok();
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty());
    terminal_status(interactive, term.as_deref(), locale.as_deref())
}

fn check_claude(cfg: &Config) -> Vec<CheckResult> {
    let binary = &cfg.claude.binary;
    let found = which::which(binary).ok();
    let mut out = Vec::new();
    match found {
        None => {
            out.push(CheckResult::fail(
                ENV,
                "claude",
                format!("`{binary}` not found on PATH"),
                "install Claude Code (npm install -g @anthropic-ai/claude-code) or set claude.binary",
            ));
            out.push(CheckResult::skipped(ENV, "claude auth", "claude binary missing"));
            return out;
        }
        Some(path) => match version_of(binary, "--version") {
            Ok(v) => out.push(CheckResult::ok(ENV, "claude", format!("{v} ({})", path.display()))),
            Err(e) => out.push(CheckResult::warn(ENV, "claude", format!("{e:#}"), "reinstall Claude Code")),
        },
    }
    match claude_auth_status(binary) {
        Ok(a) if a.logged_in => out.push(CheckResult::ok(ENV, "claude auth", a.detail)),
        Ok(a) => out.push(CheckResult::fail(
            ENV,
            "claude auth",
            format!("not logged in ({})", a.detail),
            format!("run `{binary} auth login`"),
        )),
        Err(e) => out.push(CheckResult::fail(ENV, "claude auth", format!("{e:#}"), format!("run `{binary} auth login`"))),
    }
    out
}

// ------------------------------------------------------------ configuration

fn check_config(cfg: &Config, paths: &Paths) -> CheckResult {
    let file = paths.config_file();
    if !file.exists() {
        return CheckResult::fail(CONF, "config.toml", format!("{} does not exist", file.display()), "run `powerqueue init`");
    }
    let problems = cfg.validate();
    let deprecations = cfg.deprecations();
    if problems.is_empty() && !deprecations.is_empty() {
        CheckResult::warn(
            CONF,
            "config.toml",
            format!("{} uses old budget keys: {}", file.display(), deprecations.join("; ")),
            "budgets are per provider now: move the flat [budget] keys and [budget.models.*] under [budget.providers.claude] (`powerqueue config set` rewrites them for you)",
        )
    } else if problems.is_empty() {
        CheckResult::ok(CONF, "config.toml", format!("{} is valid", file.display()))
    } else {
        CheckResult::fail(
            CONF,
            "config.toml",
            problems.join("; "),
            "fix the listed keys with `powerqueue config set <key> <value>` or `powerqueue config edit`",
        )
    }
}

fn check_repo(cfg: &Config) -> CheckResult {
    let path = cfg.repo_path();
    if cfg.repo.path.trim().is_empty() {
        return CheckResult::fail(CONF, "repository", "repo.path is not set", "run `powerqueue init` or set repo.path");
    }
    if !path.exists() {
        return CheckResult::fail(
            CONF,
            "repository",
            format!("{} does not exist", path.display()),
            "set repo.path to your checkout",
        );
    }
    if Repo::new(&path).is_repo() {
        CheckResult::ok(CONF, "repository", path.display().to_string())
    } else {
        CheckResult::fail(
            CONF,
            "repository",
            format!("{} is not a git repository", path.display()),
            "set repo.path to a git checkout",
        )
    }
}

fn check_worktree_root(cfg: &Config, paths: &Paths) -> CheckResult {
    let root = cfg.worktree_root(paths);
    let probe = root.join(".powerqueue-write-test");
    let result =
        std::fs::create_dir_all(&root).and_then(|_| std::fs::write(&probe, b"ok")).and_then(|_| std::fs::remove_file(&probe));
    match result {
        Ok(()) => CheckResult::ok(CONF, "worktree root", format!("{} is writable", root.display())),
        Err(e) => CheckResult::fail(
            CONF,
            "worktree root",
            format!("{} is not writable: {e}", root.display()),
            "set repo.worktree_root to a writable directory",
        ),
    }
}

fn check_priority_file(cfg: &Config, paths: &Paths) -> CheckResult {
    let file = cfg.priority_file(paths);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(_) => {
            return CheckResult::warn(
                CONF,
                "PRIORITY.md",
                format!("{} not found; every task gets the default criticality", file.display()),
                "run `powerqueue init` or create the file from the template (`powerqueue priority edit`)",
            );
        }
    };
    match PriorityRules::parse(&text) {
        Ok(rules) if rules.warnings.is_empty() => {
            CheckResult::ok(CONF, "PRIORITY.md", format!("{} parses cleanly", file.display()))
        }
        Ok(rules) => CheckResult::warn(
            CONF,
            "PRIORITY.md",
            rules.warnings.iter().map(|w| format!("line {}: {}", w.line, w.message)).collect::<Vec<_>>().join("; "),
            "run `powerqueue priority check` and fix the listed lines",
        ),
        Err(errors) => CheckResult::fail(
            CONF,
            "PRIORITY.md",
            errors.iter().map(|w| format!("line {}: {}", w.line, w.message)).collect::<Vec<_>>().join("; "),
            "run `powerqueue priority check` and fix the listed lines",
        ),
    }
}

fn check_repo_overrides(cfg: &Config) -> CheckResult {
    let file = cfg.repo_path().join(REPO_CONFIG_FILE);
    if !file.exists() {
        return CheckResult::ok(CONF, ".powerqueue.toml", "no repository overrides");
    }
    match std::fs::read_to_string(&file)
        .map_err(|e| e.to_string())
        .and_then(|t| toml::from_str::<RepoOverrides>(&t).map_err(|e| e.message().to_string()))
    {
        Ok(_) => CheckResult::ok(CONF, ".powerqueue.toml", format!("{} parses", file.display())),
        Err(e) => CheckResult::fail(CONF, ".powerqueue.toml", format!("{}: {e}", file.display()), "fix the override file"),
    }
}

/// How to learn a provider's period reset when nothing is known yet.
fn anchor_hint(p: Provider) -> String {
    match p {
        Provider::Claude => {
            "run `powerqueue budget set-reset <time>` with the reset `/usage` shows, or let a session's status line report it"
                .to_string()
        }
        _ => format!(
            "run `powerqueue budget probe --provider {p}` (the probe learns it) or `powerqueue budget set-reset <time> --provider {p}`"
        ),
    }
}

/// The anchor state of one provider: where the period boundary comes from.
/// `observed_reset` is the reset instant a probe reported, if any and still ahead.
pub fn anchor_status(p: Provider, configured: Option<&str>, observed_reset: Option<chrono::DateTime<Utc>>) -> CheckResult {
    let name = format!("{p} budget anchor");
    match (configured, observed_reset) {
        (_, Some(reset)) => CheckResult::ok(
            CONF,
            &name,
            format!("period reset learned from the {p} probe: {} (observed)", reset.format("%Y-%m-%d %H:%M UTC")),
        ),
        (Some(a), None) => CheckResult::ok(CONF, &name, format!("period resets anchored at {a} (config)")),
        (None, None) => CheckResult::warn(
            CONF,
            &name,
            format!(
                "budget.providers.{p}.period_anchor is not set and no probe has reported a reset; pacing assumes Monday 00:00 UTC (default)"
            ),
            anchor_hint(p),
        ),
    }
}

/// One anchor check per enabled provider: config, observed (a probe reported
/// the next reset) or the Monday default.
fn check_period_anchors(cfg: &Config, store: &Store) -> Vec<CheckResult> {
    let now = Utc::now();
    cfg.budget
        .enabled_providers_in_order()
        .into_iter()
        .map(|p| {
            let observed = load_observed(store, p).ok().flatten().and_then(|o| o.period_resets_at).filter(|r| *r > now);
            anchor_status(p, cfg.budget.provider(p).period_anchor.as_deref(), observed)
        })
        .collect()
}

/// Model table of one provider: enabled shares must sum to at most 1 and at
/// least one enabled model must have a share.
pub fn provider_models_status(cfg: &Config, p: Provider) -> CheckResult {
    let name = format!("{p} models");
    let budget = cfg.budget.provider(p);
    let enabled = budget.enabled_models();
    let share: f64 = enabled.iter().filter_map(|m| budget.models.get(m)).map(|m| m.share).sum();
    if enabled.is_empty() {
        return CheckResult::fail(
            CONF,
            &name,
            format!("budget.providers.{p} is enabled but no model is enabled with a share > 0; nothing can run on it"),
            format!(
                "enable a model (`powerqueue config set budget.providers.{p}.models.<model>.enabled true`) or disable the provider"
            ),
        );
    }
    if share > 1.0 + 1e-6 {
        return CheckResult::fail(
            CONF,
            &name,
            format!("enabled shares of budget.providers.{p}.models sum to {share:.2} (> 1.0)"),
            "lower the shares so they add up to at most 1.0".to_string(),
        );
    }
    let names: Vec<&str> = enabled.iter().map(|m| m.alias()).collect();
    CheckResult::ok(CONF, &name, format!("{} enabled model(s): {} (shares sum {share:.2})", enabled.len(), names.join(", ")))
}

fn check_provider_models(cfg: &Config) -> Vec<CheckResult> {
    cfg.budget.enabled_providers_in_order().into_iter().map(|p| provider_models_status(cfg, p)).collect()
}

/// How to install a provider's CLI.
fn install_hint(p: Provider) -> &'static str {
    match p {
        Provider::Claude => "install Claude Code (npm install -g @anthropic-ai/claude-code) or set claude.binary",
        Provider::Codex => "install Codex CLI (npm install -g @openai/codex) or set codex.binary",
        Provider::Gemini => "install the Antigravity CLI (`agy`, Google AI Pro/Ultra) or set gemini.binary",
    }
}

/// How to log in to a provider's CLI.
fn login_hint(p: Provider, binary: &str) -> String {
    match p {
        Provider::Claude => format!("run `{binary} auth login`"),
        Provider::Codex => format!("run `{binary} login` (ChatGPT sign-in; `{binary} login status` shows the state)"),
        Provider::Gemini => format!("run `{binary}` once and sign in with the Google account that has AI Pro/Ultra"),
    }
}

/// Enabled non-Claude providers: binary on PATH + version (`<p>`), logged in
/// (`<p> auth`), a valid mode, and the experimental note for `gemini`.
/// Claude has its own checks ([`check_claude`]).
fn check_providers(cfg: &Config) -> Vec<CheckResult> {
    let mut out = Vec::new();
    for p in cfg.budget.enabled_providers_in_order() {
        if p == Provider::Claude {
            continue;
        }
        let settings = cfg.launch_settings(p);
        let agent = crate::session::agent_for(p);
        let name = p.as_str();
        let auth_name = format!("{p} auth");
        match which::which(&settings.binary) {
            Err(_) => {
                out.push(CheckResult::fail(
                    ENV,
                    name,
                    format!("budget.providers.{p}.enabled is true but `{}` is not on PATH", settings.binary),
                    format!("{}, or set budget.providers.{p}.enabled = false", install_hint(p)),
                ));
                out.push(CheckResult::skipped(ENV, &auth_name, format!("{name} binary missing")));
            }
            Ok(path) => {
                match version_of(&settings.binary, "--version") {
                    Ok(v) => out.push(CheckResult::ok(ENV, name, format!("{v} ({})", path.display()))),
                    Err(e) => out.push(CheckResult::warn(ENV, name, format!("{e:#}"), install_hint(p))),
                }
                match agent.auth_status(&settings.binary) {
                    Ok(a) if a.logged_in => out.push(CheckResult::ok(ENV, &auth_name, a.detail)),
                    Ok(a) => out.push(CheckResult::fail(
                        ENV,
                        &auth_name,
                        format!(
                            "`{}` is not logged in ({}); its sessions would stop at the login prompt",
                            settings.binary, a.detail
                        ),
                        login_hint(p, &settings.binary),
                    )),
                    Err(e) => out.push(CheckResult::fail(ENV, &auth_name, format!("{e:#}"), login_hint(p, &settings.binary))),
                }
                if !agent.allowed_modes().contains(&settings.mode.as_str()) {
                    out.push(CheckResult::fail(
                        CONF,
                        name,
                        format!("{name} mode `{}` is not one of {}", settings.mode, agent.allowed_modes().join("|")),
                        "fix it in config.toml",
                    ));
                }
            }
        }
        if p == Provider::Gemini {
            out.push(CheckResult::warn(
                CONF,
                name,
                "Antigravity CLI support is experimental: the launch flags, hooks and `/usage` probe follow community reports and may break when agy changes",
                "watch `powerqueue logs` and `budget show` for gemini; set budget.providers.gemini.enabled = false if sessions misbehave",
            ));
        }
    }
    out
}

/// Interactive Claude Code blocks on a trust dialog in untrusted folders.
/// With `claude.trust_workspace` the launcher pre-seeds trust; otherwise the
/// repository root must already be trusted in `~/.claude.json`.
fn check_workspace_trust(cfg: &Config, fix: bool) -> CheckResult {
    use crate::session::trust::{claude_json_path, ensure_trusted, is_trusted};
    let file = claude_json_path();
    let repo = cfg.repo_path();
    let root = std::fs::canonicalize(&repo).unwrap_or(repo);
    if is_trusted(&file, &root) {
        return CheckResult::ok(CONF, "workspace trust", format!("{} is trusted in {}", root.display(), file.display()));
    }
    if cfg.claude.trust_workspace {
        if fix {
            return match ensure_trusted(&file, &[root.as_path()]) {
                Ok(_) => {
                    let mut r = CheckResult::ok(CONF, "workspace trust", format!("marked {} as trusted", root.display()));
                    r.fixed = true;
                    r
                }
                Err(e) => CheckResult::warn(
                    CONF,
                    "workspace trust",
                    format!("could not update {}: {e:#}", file.display()),
                    "accept the trust dialog once by running `claude` in the repository",
                ),
            };
        }
        return CheckResult::ok(
            CONF,
            "workspace trust",
            format!("{} not yet trusted; the launcher marks it before the first session", root.display()),
        );
    }
    CheckResult::warn(
        CONF,
        "workspace trust",
        format!("{} is not trusted in {} and claude.trust_workspace is false", root.display(), file.display()),
        "run `claude` in the repository once and accept the trust dialog, or set claude.trust_workspace = true",
    )
}

// ------------------------------------------------------------------ secrets

async fn check_secrets(cfg: &Config, secrets: &Secrets, online: bool) -> Vec<CheckResult> {
    let mut out = vec![CheckResult::ok(SEC, "backend", secrets.backend_description())];
    match secrets.get_with_origin(SecretKind::LinearApiKey) {
        Ok(Some((key, origin))) if cfg.linear.enabled => {
            if !online {
                out.push(CheckResult::ok(SEC, "linear key", format!("present ({origin}); not verified offline")));
            } else {
                match LinearClient::new(cfg.linear.endpoint.clone(), key) {
                    Ok(client) => match client.viewer().await {
                        Ok(v) => out.push(CheckResult::ok(
                            SEC,
                            "linear key",
                            format!("{origin}; authenticated as {} <{}>", v.name, v.email),
                        )),
                        Err(e) => out.push(CheckResult::fail(
                            SEC,
                            "linear key",
                            format!("{origin}; rejected: {e:#}"),
                            "run `powerqueue secrets set linear`",
                        )),
                    },
                    Err(e) => out.push(CheckResult::fail(SEC, "linear key", format!("{e:#}"), "check linear.endpoint")),
                }
            }
        }
        Ok(Some((_, origin))) => out.push(CheckResult::ok(SEC, "linear key", format!("present ({origin}); Linear is disabled"))),
        Ok(None) if cfg.linear.enabled => out.push(CheckResult::fail(
            SEC,
            "linear key",
            "no Linear API key configured",
            "run `powerqueue secrets set linear` or set LINEAR_API_KEY",
        )),
        Ok(None) => out.push(CheckResult::ok(SEC, "linear key", "not configured; Linear is disabled")),
        Err(e) => out.push(CheckResult::fail(
            SEC,
            "linear key",
            format!("cannot read secret: {e:#}"),
            "check the keychain / secrets file",
        )),
    }
    if cfg.priority.jev.enabled {
        match secrets.get_with_origin(SecretKind::JevApiKey) {
            Ok(Some((key, origin))) if online => {
                match JevClient::new(cfg.priority.jev.endpoint.clone(), key, cfg.priority.jev.model.clone()) {
                    Ok(client) => match client.ping().await {
                        Ok(()) => out.push(CheckResult::ok(SEC, "jev key", format!("{origin}; endpoint reachable"))),
                        Err(e) => out.push(CheckResult::fail(
                            SEC,
                            "jev key",
                            format!("{origin}; rejected: {e:#}"),
                            "run `powerqueue secrets set jev`",
                        )),
                    },
                    Err(e) => out.push(CheckResult::fail(SEC, "jev key", format!("{e:#}"), "check priority.jev.endpoint")),
                }
            }
            Ok(Some((_, origin))) => {
                out.push(CheckResult::ok(SEC, "jev key", format!("present ({origin}); not verified offline")))
            }
            Ok(None) => out.push(CheckResult::fail(
                SEC,
                "jev key",
                "priority.jev.enabled is true but no Jev API key is configured",
                "run `powerqueue secrets set jev` or disable priority.jev",
            )),
            Err(e) => out.push(CheckResult::fail(
                SEC,
                "jev key",
                format!("cannot read secret: {e:#}"),
                "check the keychain / secrets file",
            )),
        }
    } else {
        out.push(CheckResult::skipped(SEC, "jev key", "Jev scoring disabled"));
    }
    out
}

// -------------------------------------------------------------------- state

fn check_db(store: &Store) -> CheckResult {
    match store.integrity_check() {
        Ok(s) if s == "ok" => CheckResult::ok(STATE, "database", "integrity check ok"),
        Ok(s) => {
            CheckResult::fail(STATE, "database", format!("integrity check: {s}"), "back up and recreate the database (data dir)")
        }
        Err(e) => CheckResult::fail(STATE, "database", format!("{e:#}"), "check the data directory permissions"),
    }
}

fn check_daemon(store: &Store) -> CheckResult {
    match store.daemon_heartbeat() {
        Ok(Some((pid, at))) => {
            let age = (Utc::now() - at).num_seconds();
            if age < 30 {
                CheckResult::ok(STATE, "daemon", format!("running (pid {pid}, heartbeat {age}s ago)"))
            } else {
                CheckResult::warn(
                    STATE,
                    "daemon",
                    format!("not running (last heartbeat {} ago)", crate::cli::output::human_duration(age)),
                    "start it with `powerqueue run`",
                )
            }
        }
        Ok(None) => CheckResult::warn(STATE, "daemon", "never started", "start it with `powerqueue run`"),
        Err(e) => CheckResult::fail(STATE, "daemon", format!("{e:#}"), "check the database"),
    }
}

fn pane_ids(tmux: &Tmux, session: &str) -> Option<Vec<crate::tmux::PaneInfo>> {
    tmux.list_panes(session).ok()
}

fn check_stuck_tasks(cfg: &Config, store: &Store, fix: bool) -> CheckResult {
    let tmux = Tmux::new(cfg.tmux.binary.clone(), cfg.tmux.socket_name.clone());
    let panes = pane_ids(&tmux, &cfg.tmux.session_name);
    let tasks = match store.list_tasks() {
        Ok(t) => t,
        Err(e) => return CheckResult::fail(STATE, "stuck tasks", format!("{e:#}"), "check the database"),
    };
    let mut stuck = Vec::new();
    for task in tasks.into_iter().filter(|t| t.state.has_live_session()) {
        let session = store.latest_session(task.id).ok().flatten();
        let reason = match &session {
            None => Some("no session row".to_string()),
            Some(s) if !s.state.is_live() => Some(format!("session is {}", s.state)),
            Some(s) => match (&panes, &s.pane_id) {
                (Some(list), Some(pane)) => {
                    let live = list.iter().any(|p| &p.pane_id == pane && !p.dead);
                    (!live).then(|| format!("tmux pane {pane} is gone"))
                }
                (Some(_), None) => Some("session has no tmux pane".to_string()),
                (None, _) => None,
            },
        };
        if let Some(reason) = reason {
            stuck.push((task, session, reason));
        }
    }
    if stuck.is_empty() {
        return CheckResult::ok(STATE, "stuck tasks", "every live task has a live session");
    }
    let detail = stuck.iter().map(|(t, _, r)| format!("{} ({}: {r})", t.key, t.state)).collect::<Vec<_>>().join(", ");
    let mut result = CheckResult::warn(
        STATE,
        "stuck tasks",
        format!("{} task(s) look live but have no live session: {detail}", stuck.len()),
        "`powerqueue doctor --fix` marks them crashed so the daemon relaunches them",
    );
    if fix {
        let mut fixed_all = true;
        for (mut task, session, reason) in stuck {
            task.state = TaskState::Crashed;
            task.not_before = Some(Utc::now());
            task.last_error = Some(format!("doctor: {reason}"));
            if store.update_task(&task).is_err() {
                fixed_all = false;
                continue;
            }
            if let Some(mut s) = session
                && s.state.is_live()
            {
                s.state = SessionState::Crashed;
                s.ended_at = Some(Utc::now());
                let _ = store.update_session(&s);
            }
            let _ = store.log_event(
                Some(task.id),
                None,
                EventLevel::Warn,
                "task.crashed",
                &format!("doctor marked the task crashed: {reason}"),
                serde_json::json!({ "reason": reason, "by": "doctor" }),
            );
        }
        result.fixed = fixed_all;
        if fixed_all {
            result.detail.push_str(" — marked crashed; the daemon will relaunch them");
        }
    }
    result
}

fn check_orphan_worktrees(cfg: &Config, paths: &Paths, store: &Store, fix: bool) -> CheckResult {
    let repo_path = cfg.repo_path();
    if !repo_path.exists() {
        return CheckResult::skipped(STATE, "orphan worktrees", "repository missing");
    }
    let repo = Repo::new(&repo_path);
    let root = cfg.worktree_root(paths);
    let entries = match repo.list_worktrees() {
        Ok(e) => e,
        Err(e) => {
            return CheckResult::warn(
                STATE,
                "orphan worktrees",
                format!("cannot list worktrees: {e:#}"),
                "run `git worktree list` in the repo",
            );
        }
    };
    let open: Vec<_> = store.list_open_tasks().unwrap_or_default();
    let owned = |p: &Path| open.iter().any(|t| t.worktree_path.as_deref().map(Path::new) == Some(p));
    let orphans: Vec<_> = entries.into_iter().filter(|e| !e.bare && e.path.starts_with(&root) && !owned(&e.path)).collect();
    if orphans.is_empty() {
        return CheckResult::ok(STATE, "orphan worktrees", format!("no stray worktrees under {}", root.display()));
    }
    let base = cfg.repo.default_branch.clone().or_else(|| repo.default_branch().ok()).unwrap_or_else(|| "main".to_string());
    let mut kept = Vec::new();
    let mut removed = 0usize;
    for wt in &orphans {
        let dirty = repo.is_dirty(&wt.path).unwrap_or(true);
        let unpushed = wt.branch.as_deref().map(|b| repo.unpushed_commits(&wt.path, b, &base).unwrap_or(1)).unwrap_or(1);
        let safe = !dirty && unpushed == 0;
        if fix && safe {
            match repo.remove_worktree(&wt.path, true) {
                Ok(()) => removed += 1,
                Err(e) => kept.push(format!("{} (remove failed: {e:#})", wt.path.display())),
            }
        } else if safe {
            kept.push(format!("{} (clean, pushed)", wt.path.display()));
        } else {
            kept.push(format!(
                "{} ({}{})",
                wt.path.display(),
                if dirty { "dirty" } else { "clean" },
                if unpushed > 0 { format!(", {unpushed} unpushed commit(s)") } else { String::new() }
            ));
        }
    }
    if kept.is_empty() {
        let mut r = CheckResult::ok(STATE, "orphan worktrees", format!("removed {removed} stray worktree(s)"));
        r.fixed = true;
        return r;
    }
    let mut r = CheckResult::warn(
        STATE,
        "orphan worktrees",
        format!("{} worktree(s) belong to no open task: {}", kept.len(), kept.join(", ")),
        "`powerqueue doctor --fix` removes clean, pushed ones; inspect dirty/unpushed ones by hand (git worktree remove)",
    );
    if removed > 0 {
        r.detail.push_str(&format!(" (removed {removed})"));
    }
    r
}

fn check_orphan_windows(cfg: &Config, store: &Store, fix: bool) -> CheckResult {
    let tmux = Tmux::new(cfg.tmux.binary.clone(), cfg.tmux.socket_name.clone());
    let Some(panes) = pane_ids(&tmux, &cfg.tmux.session_name) else {
        return CheckResult::skipped(STATE, "orphan tmux windows", "tmux unavailable");
    };
    let open: Vec<_> = store.list_open_tasks().unwrap_or_default();
    let slugs: Vec<String> = open.iter().map(|t| t.slug()).collect();
    let orphans: Vec<_> = panes
        .iter()
        .filter(|p| {
            p.window_name != cfg.tmux.session_name && !p.window_name.is_empty() && !slugs.iter().any(|s| s == &p.window_name)
        })
        .collect();
    if orphans.is_empty() {
        return CheckResult::ok(
            STATE,
            "orphan tmux windows",
            format!("no stray windows in tmux session {}", cfg.tmux.session_name),
        );
    }
    let mut kept = Vec::new();
    let mut killed = 0usize;
    for p in orphans {
        if p.dead && fix {
            match tmux.kill_window(&p.window_id) {
                Ok(()) => killed += 1,
                Err(e) => kept.push(format!("{} (kill failed: {e:#})", p.window_name)),
            }
        } else {
            kept.push(format!("{} ({})", p.window_name, if p.dead { "dead" } else { "still running" }));
        }
    }
    if kept.is_empty() {
        let mut r = CheckResult::ok(STATE, "orphan tmux windows", format!("closed {killed} dead window(s)"));
        r.fixed = true;
        return r;
    }
    let mut r = CheckResult::warn(
        STATE,
        "orphan tmux windows",
        format!("{} window(s) match no open task: {}", kept.len(), kept.join(", ")),
        "`powerqueue doctor --fix` closes dead ones; close running ones with `tmux kill-window`",
    );
    if killed > 0 {
        r.detail.push_str(&format!(" (closed {killed})"));
    }
    r
}

/// True if a process with `pid` exists.
pub fn process_alive(pid: u32) -> bool {
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
        sysinfo::ProcessRefreshKind::nothing(),
    );
    system.process(sysinfo::Pid::from_u32(pid)).is_some()
}

fn check_stale_lock(paths: &Paths, store: &Store, fix: bool) -> CheckResult {
    let pid_file = paths.daemon_pid();
    let lock_file = paths.daemon_lock();
    if !pid_file.exists() && !lock_file.exists() {
        return CheckResult::ok(STATE, "daemon lock", "no lock or pid file");
    }
    let pid: Option<u32> = std::fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse().ok());
    let heartbeat_alive = store.daemon_alive(Duration::seconds(30)).unwrap_or(false);
    if heartbeat_alive || pid.map(process_alive).unwrap_or(false) {
        return CheckResult::ok(
            STATE,
            "daemon lock",
            format!("held by live daemon{}", pid.map(|p| format!(" (pid {p})")).unwrap_or_default()),
        );
    }
    let mut r = CheckResult::warn(
        STATE,
        "daemon lock",
        format!("stale lock/pid file{} with no live process", pid.map(|p| format!(" (pid {p})")).unwrap_or_default()),
        "`powerqueue doctor --fix` removes it",
    );
    if fix {
        let mut ok = true;
        for f in [&pid_file, &lock_file] {
            if f.exists() && std::fs::remove_file(f).is_err() {
                ok = false;
            }
        }
        r.fixed = ok;
        if ok {
            r.detail.push_str(" — removed");
        }
    }
    r
}

// ---------------------------------------------------------------- algorithm

fn check_estimator(store: &Store) -> CheckResult {
    let summaries = match store.task_usage_summaries() {
        Ok(s) => s,
        Err(e) => return CheckResult::fail(ALGO, "cost estimator", format!("{e:#}"), "check the database"),
    };
    let est = Estimator::from_summaries(&summaries);
    let (status, detail, hint) = estimator_status(est.sample_count(), est.accuracy());
    CheckResult::new(ALGO, "cost estimator", status, detail).hint(hint.unwrap_or_default())
}

fn check_rates(cfg: &Config, store: &Store) -> Vec<CheckResult> {
    let since = Utc::now() - Duration::days(7);
    let launches = store.count_events_of_kind("session.launched", since).unwrap_or(0);
    let crashes = store.count_events_of_kind("session.crashed", since).unwrap_or(0);
    let idle = store.count_events_of_kind("session.nudged", since).unwrap_or(0)
        + store.count_events_of_kind("task.needs_attention", since).unwrap_or(0)
        + store.count_events_of_kind("task.blocked", since).unwrap_or(0)
        + store.count_events_of_kind("session.permission_prompt", since).unwrap_or(0);
    let mut out = Vec::new();

    let (status, rate) = rate_status(crashes, launches, MAX_CRASH_RATE);
    out.push(
        CheckResult::new(ALGO, "crash rate", status, format!("{crashes} crash(es) over {launches} launch(es) in 7 days ({:.0}%)", rate * 100.0)).hint(
            if status == Status::Warn {
                format!(
                    "sessions die often: check claude.permission_mode (`{}`) is unattended, that repo.setup commands succeed, and raise scheduler.stale_session_secs if work is just slow",
                    cfg.claude.permission_mode
                )
            } else {
                String::new()
            },
        ),
    );
    let (status, rate) = rate_status(idle, launches, MAX_CRASH_RATE);
    out.push(
        CheckResult::new(ALGO, "idle / attention rate", status, format!("{idle} idle or needs-attention event(s) over {launches} launch(es) ({:.0}%)", rate * 100.0)).hint(
            if status == Status::Warn {
                "sessions stop without finishing: tighten the completion protocol in the prompt (always run `powerqueue task complete`), or use a less interactive permission mode".to_string()
            } else {
                String::new()
            },
        ),
    );
    let throttles = store.count_events_of_kind("task.throttled", Utc::now() - Duration::hours(24)).unwrap_or(0);
    let status = if throttles > MAX_THROTTLES_PER_DAY { Status::Warn } else { Status::Ok };
    out.push(CheckResult::new(ALGO, "throttling", status, format!("{throttles} throttle event(s) in 24 h")).hint(if status == Status::Warn {
        "tasks wait on budget often: raise budget.providers.<provider>.period_weighted_tokens / window_weighted_tokens, lower the shares of expensive models, or enable another provider"
    } else {
        ""
    }));
    out
}

/// Freshness of a provider's observed usage, as a pure function of the
/// facts: `None` observation = "no probe yet" (info), a failing probe warns,
/// an observation older than [`STALE_PROBE_INTERVALS`] intervals warns.
pub fn probe_status(
    p: Provider,
    interval_mins: u64,
    observed_age_secs: Option<i64>,
    last_error: Option<&str>,
    now_desc: &str,
) -> CheckResult {
    let name = format!("{p} usage probe");
    if interval_mins == 0 {
        return CheckResult::skipped(ALGO, &name, "usage probes are disabled (budget.probe_interval_mins = 0)");
    }
    let run_now = format!("`powerqueue budget probe --provider {p}` runs it now");
    if let Some(err) = last_error {
        return CheckResult::warn(
            ALGO,
            &name,
            format!("the last probe failed: {err}"),
            match p {
                Provider::Claude => {
                    "the status line reports usage once a session answers; nothing to fix unless sessions never start".to_string()
                }
                Provider::Codex => format!("check `codex login status` and codex.binary; {run_now}"),
                Provider::Gemini => format!("check that `agy -p /usage` works by hand and gemini.binary is right; {run_now}"),
            },
        );
    }
    let Some(age) = observed_age_secs else {
        return CheckResult::ok(
            ALGO,
            &name,
            match p {
                Provider::Claude => "no probe yet: a session's status line reports usage after its first response".to_string(),
                _ => format!("no probe yet: the daemon probes at start and every {interval_mins} min; {run_now}"),
            },
        );
    };
    let stale_after = interval_mins as i64 * 60 * STALE_PROBE_INTERVALS;
    if age > stale_after {
        return CheckResult::warn(
            ALGO,
            &name,
            format!(
                "observed usage is {} old (probes run every {interval_mins} min); pacing may be off",
                crate::cli::output::human_duration(age)
            ),
            format!("is the daemon running? {run_now}"),
        );
    }
    CheckResult::ok(ALGO, &name, format!("{now_desc}, {} ago", crate::cli::output::human_duration(age)))
}

fn check_probes(cfg: &Config, store: &Store) -> Vec<CheckResult> {
    let now = Utc::now();
    cfg.budget
        .enabled_providers_in_order()
        .into_iter()
        .map(|p| {
            let observed = load_observed(store, p).ok().flatten();
            let age = observed.as_ref().map(|o| (now - o.observed_at).num_seconds().max(0));
            let status = load_probe_status(store, p).ok().flatten();
            let error = status.as_ref().filter(|s| !s.ok).and_then(|s| s.error.clone());
            let desc = observed.as_ref().map(crate::budget::probes::describe).unwrap_or_default();
            probe_status(p, cfg.budget.probe_interval_mins, age, error.as_deref(), &desc)
        })
        .collect()
}

/// Pacing of a provider's most capable enabled model (`<model> reservation`)
/// and its window pressure (`window pressure` for Claude, `<p> window
/// pressure` otherwise).
fn ledger_checks(cfg: &Config, ledger: &Ledger) -> Vec<CheckResult> {
    let p = ledger.provider;
    let mut out = Vec::new();
    let elapsed = ledger.elapsed_fraction();
    if let Some(top) = cfg.budget.provider(p).enabled_models().into_iter().next() {
        let name = format!("{} reservation", top.alias());
        let spent = ledger.tier(&top).period_spent_fraction();
        match top_model_pacing(&top, elapsed, spent) {
            Some((status, detail, hint)) => out.push(CheckResult::new(ALGO, &name, status, detail).hint(hint)),
            None => out.push(CheckResult::ok(
                ALGO,
                &name,
                format!("{:.0}% of {top}'s share spent, {:.0}% of the {p} period elapsed", spent * 100.0, elapsed * 100.0),
            )),
        }
    }
    let name = if p == Provider::Claude { "window pressure".to_string() } else { format!("{p} window pressure") };
    let window = ledger.window_fraction();
    let window_hours = cfg.budget.provider(p).window_hours;
    if !ledger.window_enabled {
        out.push(CheckResult::ok(ALGO, &name, format!("no rolling window configured (budget.providers.{p}.window_hours = 0)")));
    } else if window > MAX_WINDOW_FRACTION {
        out.push(CheckResult::warn(
            ALGO,
            &name,
            format!("{:.0}% of the {p} {window_hours}h window budget is spent", window * 100.0),
            format!(
                "lower scheduler.max_concurrent or raise budget.providers.{p}.window_weighted_tokens if the provider shows headroom"
            ),
        ));
    } else {
        out.push(CheckResult::ok(ALGO, &name, format!("{:.0}% of the {p} {window_hours}h window budget spent", window * 100.0)));
    }
    out
}

fn check_ledger(cfg: &Config, store: &Store) -> Vec<CheckResult> {
    let now = Utc::now();
    let ledgers = match Ledgers::load(store, &cfg.budget, now) {
        Ok(l) => l,
        Err(e) => {
            return vec![CheckResult::warn(ALGO, "budget ledger", format!("cannot load ledger: {e:#}"), "check the usage table")];
        }
    };
    if ledgers.is_empty() {
        return vec![CheckResult::fail(
            ALGO,
            "budget ledger",
            "no provider is enabled; nothing can run",
            "set budget.providers.claude.enabled = true (or enable codex / gemini)",
        )];
    }
    ledgers.ordered(&cfg.budget.provider_order).into_iter().flat_map(|l| ledger_checks(cfg, l)).collect()
}

/// Run every check. `fix` applies safe repairs. `online` allows network calls.
pub async fn run_all(
    cfg: &Config,
    paths: &Paths,
    store: &Store,
    secrets: &Secrets,
    fix: bool,
    online: bool,
) -> Result<Vec<CheckResult>> {
    let mut results = Vec::new();
    results.push(check_git());
    results.push(check_tmux(cfg));
    results.extend(check_claude(cfg));
    results.push(check_terminal());

    results.push(check_config(cfg, paths));
    results.push(check_repo(cfg));
    results.push(check_worktree_root(cfg, paths));
    results.push(check_priority_file(cfg, paths));
    results.push(check_repo_overrides(cfg));
    results.extend(check_provider_models(cfg));
    results.extend(check_period_anchors(cfg, store));
    results.extend(check_providers(cfg));
    results.push(check_workspace_trust(cfg, fix));

    results.extend(check_secrets(cfg, secrets, online).await);

    results.push(check_db(store));
    results.push(check_daemon(store));
    results.push(check_stuck_tasks(cfg, store, fix));
    results.push(check_orphan_worktrees(cfg, paths, store, fix));
    results.push(check_orphan_windows(cfg, store, fix));
    results.push(check_stale_lock(paths, store, fix));

    results.push(check_estimator(store));
    results.extend(check_rates(cfg, store));
    results.extend(check_probes(cfg, store));
    results.extend(check_ledger(cfg, store));
    Ok(results)
}

/// Exit code: 0 all ok/warn, 1 if any failure.
pub fn exit_code(results: &[CheckResult]) -> i32 {
    if results.iter().any(|r| r.status == Status::Fail) { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_check_levels() {
        assert_eq!(terminal_status(false, None, None).status, Status::Skipped);
        let r = terminal_status(true, None, Some("en_US.UTF-8"));
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("TERM is not set"), "{}", r.detail);
        let r = terminal_status(true, Some("dumb"), Some("en_US.UTF-8"));
        assert_eq!(r.status, Status::Warn);
        let r = terminal_status(true, Some("xterm-ghostty"), Some("C"));
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("ASCII"), "{}", r.detail);
        let r = terminal_status(true, Some("xterm-ghostty"), Some("en_US.UTF-8"));
        assert_eq!(r.status, Status::Ok);
        assert_eq!(terminal_status(true, Some("xterm-ghostty"), None).status, Status::Ok, "unset locale assumed UTF-8");
    }

    #[test]
    fn rate_thresholds() {
        assert_eq!(rate_status(0, 0, 0.3), (Status::Ok, 0.0));
        assert_eq!(rate_status(1, 10, 0.3).0, Status::Ok);
        assert_eq!(rate_status(4, 10, 0.3).0, Status::Warn);
        assert!((rate_status(4, 10, 0.3).1 - 0.4).abs() < 1e-9);
    }

    #[test]
    fn top_model_pacing_advice() {
        assert!(fable_pacing(0.5, 0.5).is_none());
        let (s, d, h) = fable_pacing(0.8, 0.1).unwrap();
        assert_eq!(s, Status::Warn);
        assert!(d.contains("fable under-used"), "{d}");
        assert!(h.contains("budget.providers.claude.models.fable.relax_after_fraction"), "{h}");
        let (_, d, h) = fable_pacing(0.2, 0.6).unwrap();
        assert!(d.contains("over-paced"));
        assert!(h.contains("min_criticality"));
        assert!(fable_pacing(0.9, 0.95).is_none());
        let sol = ModelTier::new("gpt-6.1-sol");
        let (_, d, h) = top_model_pacing(&sol, 0.8, 0.1).unwrap();
        assert!(d.starts_with("gpt-6.1-sol under-used"), "{d}");
        assert!(h.contains("budget.providers.codex.models.gpt-6.1-sol.relax_after_fraction"), "{h}");
    }

    #[test]
    fn probe_freshness_levels() {
        assert_eq!(probe_status(Provider::Codex, 0, None, None, "").status, Status::Skipped);
        let r = probe_status(Provider::Codex, 15, None, None, "");
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("no probe yet"), "{}", r.detail);
        let r = probe_status(Provider::Claude, 15, None, None, "");
        assert!(r.detail.contains("status line"), "{}", r.detail);
        let r = probe_status(Provider::Codex, 15, Some(60), Some("exit 1"), "window 10%");
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("exit 1") && r.fix_hint.as_deref().unwrap_or_default().contains("codex login status"), "{r:?}");
        let r = probe_status(Provider::Codex, 15, Some(15 * 60 * 3 + 1), None, "window 10%");
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("old"), "{}", r.detail);
        let r = probe_status(Provider::Codex, 15, Some(600), None, "window 10%");
        assert_eq!(r.status, Status::Ok);
        assert_eq!(r.detail, "window 10%, 10m00s ago");
    }

    #[test]
    fn anchor_sources() {
        let r = anchor_status(Provider::Codex, None, None);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("(default)"), "{}", r.detail);
        assert!(r.fix_hint.as_deref().unwrap_or_default().contains("budget probe --provider codex"), "{r:?}");
        let r = anchor_status(Provider::Claude, None, None);
        assert!(r.fix_hint.as_deref().unwrap_or_default().contains("budget set-reset <time>"), "{r:?}");
        let r = anchor_status(Provider::Claude, Some("2026-03-02T09:00:00Z"), None);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.ends_with("(config)"), "{}", r.detail);
        let r = anchor_status(Provider::Codex, None, Some(Utc::now() + Duration::days(2)));
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.ends_with("(observed)"), "{}", r.detail);
    }

    #[test]
    fn provider_model_tables() {
        let mut cfg = Config::default();
        let r = provider_models_status(&cfg, Provider::Claude);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("fable, opus, sonnet, haiku"), "{}", r.detail);
        cfg.budget.providers.codex.models.get_mut(&ModelTier::new("gpt-6.1-sol")).unwrap().share = 0.9;
        assert_eq!(provider_models_status(&cfg, Provider::Codex).status, Status::Fail);
        for m in cfg.budget.providers.gemini.models.values_mut() {
            m.enabled = false;
        }
        let r = provider_models_status(&cfg, Provider::Gemini);
        assert_eq!(r.status, Status::Fail);
        assert!(r.detail.contains("no model is enabled"), "{}", r.detail);
    }

    #[test]
    fn ledger_checks_per_provider() {
        let store = Store::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.budget.providers.codex.enabled = true;
        cfg.budget.providers.codex.window_hours = 0;
        let results = check_ledger(&cfg, &store);
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["fable reservation", "window pressure", "gpt-6.1-sol reservation", "codex window pressure"]);
        assert!(results[3].detail.contains("no rolling window"), "{}", results[3].detail);
        cfg.budget.providers.claude.enabled = false;
        cfg.budget.providers.codex.enabled = false;
        let results = check_ledger(&cfg, &store);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, Status::Fail);
        let probes = check_probes(&cfg, &store);
        assert!(probes.is_empty());
    }

    #[test]
    fn estimator_advice() {
        assert_eq!(estimator_status(2, None).0, Status::Warn);
        assert_eq!(estimator_status(8, Some(0.2)).0, Status::Ok);
        let (s, _, hint) = estimator_status(8, Some(0.9));
        assert_eq!(s, Status::Warn);
        assert!(hint.unwrap().contains("estimates"));
        assert_eq!(estimator_status(8, None).0, Status::Ok);
    }

    #[test]
    fn claude_auth_parsing() {
        let a = parse_claude_auth(true, r#"{"loggedIn":true,"authMethod":"claude.ai","email":"me@x.io"}"#, "");
        assert!(a.logged_in);
        assert_eq!(a.method.as_deref(), Some("claude.ai"));
        assert_eq!(a.detail, "me@x.io via claude.ai");
        let b = parse_claude_auth(true, r#"{"loggedIn":false}"#, "");
        assert!(!b.logged_in);
        let c = parse_claude_auth(false, "", "Not logged in\nrun claude auth login");
        assert!(!c.logged_in);
        assert_eq!(c.detail, "Not logged in");
        let d = parse_claude_auth(true, "not json", "");
        assert!(d.logged_in);
    }

    #[test]
    fn exit_code_and_builders() {
        let ok = CheckResult::ok("x", "y", "fine");
        let warn = CheckResult::warn("x", "y", "meh", "do this");
        assert_eq!(exit_code(&[ok.clone(), warn.clone()]), 0);
        let fail = CheckResult::fail("x", "y", "bad", "fix");
        assert_eq!(exit_code(&[ok, warn, fail]), 1);
        assert_eq!(CheckResult::ok("x", "y", "z").hint("").fix_hint, None);
    }

    #[test]
    fn offline_state_checks_on_empty_db() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(check_db(&store).status, Status::Ok);
        assert_eq!(check_daemon(&store).status, Status::Warn);
        store.heartbeat(std::process::id()).unwrap();
        assert_eq!(check_daemon(&store).status, Status::Ok);
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        assert_eq!(check_stale_lock(&paths, &store, false).status, Status::Ok);
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::write(paths.daemon_pid(), "999999999").unwrap();
        let fresh = Store::open_in_memory().unwrap();
        let r = check_stale_lock(&paths, &fresh, true);
        assert_eq!(r.status, Status::Warn);
        assert!(r.fixed);
        assert!(!paths.daemon_pid().exists());
    }

    #[test]
    fn config_checks() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let mut cfg = Config::default();
        assert_eq!(check_config(&cfg, &paths).status, Status::Fail);
        cfg.repo.path = dir.path().display().to_string();
        cfg.save(&paths).unwrap();
        assert_eq!(check_config(&cfg, &paths).status, Status::Ok);
        let store = Store::open_in_memory().unwrap();
        let anchors = check_period_anchors(&cfg, &store);
        assert_eq!(anchors.len(), 1, "one check per enabled provider: {anchors:?}");
        assert_eq!(anchors[0].status, Status::Warn);
        cfg.budget.providers.claude.period_anchor = Some("2026-03-02T09:00:00Z".into());
        assert_eq!(check_period_anchors(&cfg, &store)[0].status, Status::Ok);
        cfg.budget.providers.codex.enabled = true;
        let anchors = check_period_anchors(&cfg, &store);
        assert_eq!(anchors.len(), 2);
        assert!(anchors[1].name.contains("codex"), "{anchors:?}");
        assert_eq!(anchors[1].status, Status::Warn);
        let mut obs = crate::budget::ObservedUsage::empty(Utc::now());
        obs.period_resets_at = Some(Utc::now() + Duration::days(3));
        crate::budget::save_observed(&store, Provider::Codex, &obs).unwrap();
        let anchors = check_period_anchors(&cfg, &store);
        assert_eq!(anchors[1].status, Status::Ok, "{anchors:?}");
        assert!(anchors[1].detail.contains("observed"), "{anchors:?}");
        let models = check_provider_models(&cfg);
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|m| m.status == Status::Ok), "{models:?}");
        assert_eq!(check_worktree_root(&cfg, &paths).status, Status::Ok);
        assert_eq!(check_repo_overrides(&cfg).status, Status::Ok);
        std::fs::write(dir.path().join(REPO_CONFIG_FILE), "nope = 1").unwrap();
        assert_eq!(check_repo_overrides(&cfg).status, Status::Fail);
        assert_eq!(check_priority_file(&cfg, &paths).status, Status::Warn);
        assert!(process_alive(std::process::id()));
    }
}
