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
//!   per provider: model shares, period anchor known (config / observed / default),
//!   `powerqueue tune` drafts waiting to be applied or left behind by a failed run
//! * secrets: keychain backend, Linear key works (`viewer`), Jev key (if enabled)
//! * state: database integrity, daemon heartbeat, the systemd/launchd service (installed,
//!   running, keeps tmux sessions on stop, binary and PATH still right), orphaned worktrees / tmux windows,
//!   tasks stuck in `starting`/`running` with no live session
//! * algorithm: estimator accuracy, crash rate, idle rate, throttling frequency,
//!   per provider: probe freshness, top-model under/over-reservation, window
//!   pressure — each with a concrete suggestion

use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::budget::{Estimator, Ledger, Ledgers, load_observations, load_observed, load_probe_status, tier_weight};
use crate::cli::output::human_f64;
use crate::config::{Config, REPO_CONFIG_FILE, RepoOverrides};
use crate::domain::{EventLevel, ModelTier, Provider, SessionState, Task, TaskState};
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
/// cannot be executed at all (or `claude.binary` is a per-task command that
/// cannot run outside a task).
pub fn claude_auth_status(binary: &str) -> Result<ClaudeAuth> {
    let out = crate::session::host_command(binary)?
        .args(["auth", "status"])
        .output()
        .with_context(|| format!("run `{binary} auth status`"))?;
    Ok(parse_claude_auth(out.status.success(), &String::from_utf8_lossy(&out.stdout), &String::from_utf8_lossy(&out.stderr)))
}

fn version_of(binary: &str, flag: &str) -> Result<String> {
    let out = crate::session::host_command(binary)?.arg(flag).output().with_context(|| format!("run `{binary} {flag}`"))?;
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

/// The GitHub CLI the PR watcher runs: installed and logged in. Missing is
/// a failure only while tasks wait in review (nothing would ever advance
/// them); otherwise a warning, since `task complete --pr` needs it.
fn check_gh(cfg: &Config, store: &Store) -> CheckResult {
    const NAME: &str = "gh";
    if cfg.scheduler.pr_poll_secs == 0 {
        return CheckResult::skipped(ENV, NAME, "PR watcher disabled (scheduler.pr_poll_secs = 0)");
    }
    let in_review = store.list_tasks_in_states(&[TaskState::InReview]).map(|t| t.len()).unwrap_or(0);
    let bin = cfg.scheduler.gh_binary.as_str();
    let problem = |detail: String, hint: &str| {
        if in_review > 0 {
            CheckResult::fail(ENV, NAME, format!("{detail}; {in_review} task(s) in review cannot advance"), hint)
        } else {
            CheckResult::warn(ENV, NAME, format!("{detail}; `task complete --pr` hand-offs would never be watched"), hint)
        }
    };
    let version = match version_of(bin, "--version") {
        Ok(v) => v,
        Err(e) => {
            return problem(format!("{e:#}"), "install the GitHub CLI (https://cli.github.com) or set scheduler.gh_binary");
        }
    };
    match std::process::Command::new(bin).args(["auth", "status"]).stdin(std::process::Stdio::null()).output() {
        Ok(out) if out.status.success() => CheckResult::ok(ENV, NAME, version),
        Ok(out) => problem(
            format!("`{bin} auth status` failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
            "run `gh auth login` as the user the daemon runs as",
        ),
        Err(e) => problem(format!("cannot run `{bin} auth status`: {e}"), "check scheduler.gh_binary"),
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

/// The program of a per-task `binary` template (one with placeholders),
/// which can only be run for a launch, not from here.
fn per_task_binary(binary: &str) -> Option<String> {
    crate::session::BinaryTemplate::parse(binary).ok().filter(|t| t.is_per_task()).map(|t| t.program().to_string())
}

fn check_claude(cfg: &Config) -> Vec<CheckResult> {
    let binary = &cfg.claude.binary;
    let program = crate::session::program_of(binary);
    let mut out = Vec::new();
    if crate::session::program_is_per_task(binary) {
        out.push(CheckResult::ok(ENV, "claude", format!("per-task command `{binary}`; its program is resolved at launch")));
        out.push(CheckResult::skipped(
            ENV,
            "claude auth",
            "claude.binary is a per-task command; check the login where it runs (e.g. inside the container)",
        ));
        return out;
    }
    let found = crate::session::which_program(binary);
    match found {
        None => {
            out.push(CheckResult::fail(
                ENV,
                "claude",
                format!("`{program}` not found on PATH"),
                "install Claude Code (npm install -g @anthropic-ai/claude-code) or set claude.binary",
            ));
            out.push(CheckResult::skipped(ENV, "claude auth", "claude binary missing"));
            return out;
        }
        Some(path) if per_task_binary(binary).is_some() => {
            out.push(CheckResult::ok(ENV, "claude", format!("per-task command `{binary}` (`{program}` at {})", path.display())));
            out.push(CheckResult::skipped(
                ENV,
                "claude auth",
                "claude.binary is a per-task command; check the login where it runs (e.g. inside the container)",
            ));
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

/// `prompt.template` (or the repo's `prompt_template`) must be readable, or
/// every session silently gets the built-in prompt.
fn check_prompt_template(cfg: &Config) -> CheckResult {
    let Some(path) = cfg.prompt.template_path() else {
        return CheckResult::ok(CONF, "prompt template", "built-in prompt (no prompt.template configured)");
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let vars: std::collections::BTreeSet<&str> = crate::session::PROMPT_PLACEHOLDERS.into_iter().collect();
            let (_, unknown) = crate::session::render_template(
                &text,
                &vars.iter().map(|v| (*v, String::new())).collect::<std::collections::BTreeMap<&'static str, String>>(),
            );
            let mentions_protocol = text.contains("{{completion_protocol}}")
                || text.contains("{{default_prompt}}")
                || text.contains("powerqueue task complete");
            if !unknown.is_empty() {
                CheckResult::warn(
                    CONF,
                    "prompt template",
                    format!("{} uses unknown placeholders: {}", path.display(), unknown.join(", ")),
                    "they are left as-is in the prompt; see the placeholder table under [prompt] in the README",
                )
            } else if !mentions_protocol {
                CheckResult::warn(
                    CONF,
                    "prompt template",
                    format!("{} has no {{{{completion_protocol}}}}; sessions cannot report completion", path.display()),
                    "add {{completion_protocol}} (or the `powerqueue task complete` command) to the template",
                )
            } else {
                CheckResult::ok(CONF, "prompt template", format!("{} renders cleanly", path.display()))
            }
        }
        Err(e) => CheckResult::fail(
            CONF,
            "prompt template",
            format!("cannot read {}: {e}; sessions get the built-in prompt (event prompt.template_error)", path.display()),
            "fix prompt.template in config.toml (or prompt_template in .powerqueue.toml), or remove it",
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
/// `powerqueue tune` drafts: a proposal nobody applied, or a run that failed
/// or produced invalid files, is worth a look (the directory explains why).
fn check_tune_drafts(paths: &Paths) -> CheckResult {
    let drafts = match crate::tune::Draft::list(paths) {
        Ok(d) => d,
        Err(e) => {
            return CheckResult::warn(CONF, "tune drafts", format!("{e:#}"), "fix or remove the unreadable draft directory");
        }
    };
    tune_drafts_status(&drafts)
}

/// The verdict for a list of drafts (newest last), separated for tests.
pub fn tune_drafts_status(drafts: &[crate::tune::Draft]) -> CheckResult {
    use crate::tune::DraftStatus;
    let proposed: Vec<&crate::tune::Draft> = drafts.iter().filter(|d| d.meta.status == DraftStatus::Proposed).collect();
    let broken: Vec<&crate::tune::Draft> = drafts
        .iter()
        .filter(|d| matches!(d.meta.status, DraftStatus::Failed | DraftStatus::Invalid | DraftStatus::Running))
        .collect();
    if let Some(latest) = proposed.last() {
        return CheckResult::warn(
            CONF,
            "tune drafts",
            format!(
                "{} proposal(s) not applied; newest: {} (\"{}\")",
                proposed.len(),
                latest.dir.display(),
                crate::cli::output::truncate(&latest.meta.instruction, 50)
            ),
            "review with `powerqueue tune --apply` (or `--apply <dir>`), or delete the draft directory",
        );
    }
    if let Some(latest) = broken.last() {
        let why = latest.meta.problems.first().cloned().unwrap_or_else(|| latest.meta.status.to_string());
        return CheckResult::warn(
            CONF,
            "tune drafts",
            format!("{} draft(s) {} ; newest: {} ({why})", broken.len(), "failed or invalid", latest.dir.display()),
            "read result.json / stderr.log in the draft directory, then delete it; `powerqueue tune` prunes old finished drafts itself",
        );
    }
    CheckResult::ok(CONF, "tune drafts", format!("{} draft(s), none pending", drafts.len()))
}

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
        let program = crate::session::program_of(&settings.binary);
        if crate::session::program_is_per_task(&settings.binary) {
            out.push(CheckResult::ok(
                ENV,
                name,
                format!("per-task command `{}`; its program is resolved at launch", settings.binary),
            ));
            out.push(CheckResult::skipped(
                ENV,
                &auth_name,
                format!("{p}.binary is a per-task command; check the login where it runs (e.g. inside the container)"),
            ));
            if !agent.allowed_modes().contains(&settings.mode.as_str()) {
                out.push(CheckResult::fail(
                    CONF,
                    name,
                    format!("{name} mode `{}` is not one of {}", settings.mode, agent.allowed_modes().join("|")),
                    "fix it in config.toml",
                ));
            }
        } else {
            match crate::session::which_program(&settings.binary) {
                None => {
                    out.push(CheckResult::fail(
                        ENV,
                        name,
                        format!("budget.providers.{p}.enabled is true but `{program}` is not on PATH"),
                        format!("{}, or set budget.providers.{p}.enabled = false", install_hint(p)),
                    ));
                    out.push(CheckResult::skipped(ENV, &auth_name, format!("{name} binary missing")));
                }
                Some(path) if per_task_binary(&settings.binary).is_some() => {
                    out.push(CheckResult::ok(
                        ENV,
                        name,
                        format!("per-task command `{}` (`{program}` at {})", settings.binary, path.display()),
                    ));
                    out.push(CheckResult::skipped(
                        ENV,
                        &auth_name,
                        format!("{p}.binary is a per-task command; check the login where it runs (e.g. inside the container)"),
                    ));
                    if !agent.allowed_modes().contains(&settings.mode.as_str()) {
                        out.push(CheckResult::fail(
                            CONF,
                            name,
                            format!("{name} mode `{}` is not one of {}", settings.mode, agent.allowed_modes().join("|")),
                            "fix it in config.toml",
                        ));
                    }
                }
                Some(path) => {
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

/// With `<provider>.shim = true` sessions talk to the daemon through
/// `<task dir>/inbox`; a message the daemon could not parse is parked in
/// `inbox/rejected/`. Reports those (the shim and the daemon disagree,
/// or something else wrote there). `None` when no provider uses the shim
/// and nothing is parked.
fn check_session_inbox(cfg: &Config, paths: &Paths) -> Option<CheckResult> {
    let rejected = crate::session::inbox::rejected_counts(&paths.tasks_dir());
    if rejected.is_empty() {
        return cfg.shim_enabled().then(|| {
            CheckResult::ok(
                CONF,
                "session inbox",
                format!(
                    "sessions reach the daemon through <task dir>/{} (no rejected messages)",
                    crate::session::inbox::INBOX_RELATIVE
                ),
            )
        });
    }
    let total: usize = rejected.iter().map(|(_, n)| n).sum();
    let dirs: Vec<String> = rejected.iter().take(5).map(|(d, n)| format!("{d} ({n})")).collect();
    Some(CheckResult::warn(
        CONF,
        "session inbox",
        format!(
            "{total} message{} from session shims could not be parsed and sit in {}/<task>/{}/{}: {}",
            if total == 1 { "" } else { "s" },
            paths.tasks_dir().display(),
            crate::session::inbox::INBOX_RELATIVE,
            crate::session::inbox::REJECTED_RELATIVE,
            dirs.join(", ")
        ),
        "inspect the files (`inbox.rejected` events name them); delete them once understood. A shim older than this \
         daemon is replaced at the task's next launch",
    ))
}

/// Interactive Claude Code blocks on a trust dialog in untrusted folders.
/// With `claude.trust_workspace` the launcher pre-seeds trust; otherwise the
/// repository root must already be trusted in `~/.claude.json`.
fn check_workspace_trust(cfg: &Config, fix: bool) -> CheckResult {
    use crate::session::trust::{claude_json_path, ensure_trusted, is_trusted};
    let file = claude_json_path(cfg);
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
    if cfg.github.enabled {
        let hint = "run `powerqueue secrets set github` or set GITHUB_TOKEN; check github.repository and token Issues permissions (read for intake, write for lifecycle updates)";
        match secrets.get_with_origin(SecretKind::GitHubToken) {
            Ok(Some((key, origin))) if online => {
                let result = async {
                    let client = crate::github::GitHubClient::new(&cfg.github.endpoint, key)?;
                    client.test_repository(&cfg.github.repository).await?;
                    let mut probe = cfg.github.clone();
                    probe.max_issues = 1;
                    client.fetch_issues(&probe).await?;
                    anyhow::Ok(())
                }
                .await;
                match result {
                    Ok(()) => {
                        out.push(CheckResult::ok(SEC, "github token", format!("{origin}; repository and Issues API accessible")))
                    }
                    Err(e) => out.push(CheckResult::fail(SEC, "github token", format!("{e:#}"), hint)),
                }
            }
            Ok(Some((_, origin))) => {
                out.push(CheckResult::ok(SEC, "github token", format!("present ({origin}); not verified offline")))
            }
            Ok(None) => out.push(CheckResult::fail(SEC, "github token", "no GitHub token configured", hint)),
            Err(e) => out.push(CheckResult::fail(SEC, "github token", format!("cannot read token: {e:#}"), hint)),
        }
    } else {
        out.push(CheckResult::skipped(SEC, "github token", "GitHub disabled"));
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

/// The user service from `powerqueue service install`: running, and its unit
/// still points at a binary and a PATH that work. Skipped when there is no
/// service manager or no unit.
fn check_service(cfg: &Config) -> CheckResult {
    use crate::service::{InstalledUnit, Manager, query, required_tools, unit_problems};
    const NAME: &str = "service";
    let Ok(manager) = Manager::detect() else {
        return CheckResult::skipped(STATE, NAME, "no systemd or launchd");
    };
    let unit = match InstalledUnit::load(manager) {
        Ok(Some(u)) => u,
        Ok(None) => {
            return CheckResult::skipped(STATE, NAME, "not installed")
                .hint("`powerqueue service install` keeps the daemon running and restarts it after a crash");
        }
        Err(e) => return CheckResult::warn(STATE, NAME, format!("{e:#}"), "check the file permissions"),
    };
    let exe = std::env::current_exe().ok();
    let problems = unit_problems(&unit, exe.as_deref(), &required_tools(cfg));
    if let Some(first) = problems.first() {
        let details: Vec<&str> = problems.iter().map(|p| p.detail.as_str()).collect();
        return CheckResult::warn(STATE, NAME, format!("{manager}: {}", details.join("; ")), first.hint.clone());
    }
    match query(manager) {
        Ok(s) if s.running => {
            let mut r = CheckResult::ok(STATE, NAME, format!("{manager}: running ({})", s.detail));
            if s.linger == Some(false) {
                r = r.hint("it stops when you log out; `powerqueue service install --linger` keeps it up on a server");
            }
            r
        }
        Ok(s) => CheckResult::warn(
            STATE,
            NAME,
            format!("{manager}: installed but not running ({})", s.detail),
            "`powerqueue service start`; `powerqueue service logs` shows why it stopped",
        ),
        Err(e) => CheckResult::warn(STATE, NAME, format!("{manager}: {e:#}"), "check that the user service manager is reachable"),
    }
}

/// `powerqueue pause` in effect: nothing launches until `resume`.
fn check_scheduling_pause(store: &Store) -> CheckResult {
    match crate::domain::SchedulingPause::load(store) {
        Ok(Some(p)) => CheckResult::warn(
            STATE,
            "scheduling",
            format!("{}; queued tasks wait and crashed sessions are not relaunched", p.describe()),
            "`powerqueue resume` when you want new sessions again",
        ),
        Ok(None) => CheckResult::ok(STATE, "scheduling", "not paused"),
        Err(e) => CheckResult::fail(STATE, "scheduling", format!("{e:#}"), "check the database"),
    }
}

/// Linear dependencies: which tasks are `blocked` and on what, `blocked by`
/// cycles between open tasks (they would wait forever), and failed attempts
/// to close parent issues (`parent_errors`, `linear.parent_error` events in
/// the last day). `parent_closing` is false when `linear.manage_states` is
/// off or `linear.done_state_parent` is empty.
pub fn dependency_status(tasks: &[Task], parent_errors: u64, parent_closing: bool) -> CheckResult {
    const NAME: &str = "dependencies";
    let open: Vec<&Task> = tasks.iter().filter(|t| !t.state.is_terminal()).collect();
    let cycles = blocker_cycles(&open);
    if !cycles.is_empty() {
        return CheckResult::fail(
            STATE,
            NAME,
            format!("`blocked by` cycle between open tasks: {}; none of them can ever start", cycles.join("; ")),
            "remove one of the `blocks` relations in Linear",
        );
    }
    if parent_errors > 0 {
        return CheckResult::warn(
            STATE,
            NAME,
            format!("{parent_errors} failed attempt(s) to close a parent issue in the last 24h"),
            "check that `linear.done_state_parent` names a workflow state of the parent's team (see `powerqueue logs --events`, kind `linear.parent_error`)",
        );
    }
    let abandoned: Vec<&str> = open.iter().filter(|t| t.container_all_canceled()).map(|t| t.key.as_str()).collect();
    if !abandoned.is_empty() {
        return CheckResult::warn(
            STATE,
            NAME,
            format!("parent issue(s) whose sub-issues were all canceled, never closed by powerqueue: {}", abandoned.join(", ")),
            "close (or cancel) them in Linear, or reopen or add a sub-issue",
        );
    }
    let blocked: Vec<String> = open
        .iter()
        .filter(|t| t.state == TaskState::Blocked)
        .map(|t| {
            let on = t.waiting_on();
            if t.is_container() {
                format!("{} (parent; open: {})", t.key, if on.is_empty() { "none".to_string() } else { on.join(", ") })
            } else {
                format!("{} ← {}", t.key, on.join(", "))
            }
        })
        .collect();
    let containers = open.iter().filter(|t| t.is_container()).count();
    if containers > 0 && !parent_closing {
        return CheckResult::warn(
            STATE,
            NAME,
            format!(
                "{containers} parent issue(s) queued but parents are never moved (manage_states off or done_state_parent empty)"
            ),
            "set `linear.done_state_parent` (default \"Done\") and `linear.manage_states = true`, or close parents by hand",
        );
    }
    if blocked.is_empty() {
        CheckResult::ok(STATE, NAME, "no task waits on another issue")
    } else {
        CheckResult::ok(STATE, NAME, format!("{} blocked: {}", blocked.len(), blocked.join("; ")))
    }
}

/// `blocked by` cycles among `tasks` whose blockers are still pending, as
/// `A → B → A` strings (each cycle once).
fn blocker_cycles(tasks: &[&Task]) -> Vec<String> {
    use std::collections::{BTreeSet, HashMap};
    let edges: HashMap<&str, Vec<&str>> =
        tasks.iter().map(|t| (t.key.as_str(), t.pending_blockers().iter().map(|b| b.key.as_str()).collect())).collect();
    let mut seen: BTreeSet<Vec<&str>> = BTreeSet::new();
    let mut out = Vec::new();
    for start in edges.keys().copied() {
        // Depth-first walk from `start` looking for a path back to it.
        let mut stack: Vec<(&str, Vec<&str>)> = vec![(start, vec![start])];
        while let Some((node, path)) = stack.pop() {
            for &next in edges.get(node).map(Vec::as_slice).unwrap_or(&[]) {
                if next == start {
                    let mut canon = path.clone();
                    canon.sort_unstable();
                    if seen.insert(canon) {
                        out.push(format!("{} → {start}", path.join(" → ")));
                    }
                } else if !path.contains(&next) && edges.contains_key(next) {
                    let mut p = path.clone();
                    p.push(next);
                    stack.push((next, p));
                }
            }
        }
    }
    out.sort();
    out
}

/// Tasks handed off for review: how many are watched, which the watcher
/// parked for a human (PR closed, stale, review rounds used up), and
/// whether the watcher itself failed (`watcher_errors`: `review.error`
/// events in the last day).
pub fn review_status(tasks: &[Task], watcher_errors: u64, rounds_max: u32) -> CheckResult {
    const NAME: &str = "reviews";
    let parked: Vec<String> = tasks
        .iter()
        .filter(|t| t.state == TaskState::NeedsAttention && t.pr_url.is_some() && t.review.is_some())
        .map(|t| format!("{} ({})", t.key, t.last_error.as_deref().unwrap_or("needs a human")))
        .collect();
    if !parked.is_empty() {
        return CheckResult::warn(
            STATE,
            NAME,
            format!("{} task(s) parked by the PR watcher: {}", parked.len(), parked.join("; ")),
            format!(
                "look at the PR; `powerqueue task resume <key>` watches it again, `task retry <key>` runs one more review round (rounds max {rounds_max}: scheduler.review_rounds_max)"
            ),
        );
    }
    let review: Vec<&Task> = tasks.iter().filter(|t| t.state == TaskState::InReview).collect();
    if watcher_errors > 0 {
        return CheckResult::warn(
            STATE,
            NAME,
            format!("{watcher_errors} PR watcher error(s) in the last 24h; {} task(s) in review", review.len()),
            "see `powerqueue logs --events` (kind `review.error`); usually gh is not logged in or lacks access to the repository",
        );
    }
    if review.is_empty() {
        return CheckResult::ok(STATE, NAME, "no task in review");
    }
    let manual = review.iter().filter(|t| t.review.as_ref().is_some_and(|r| r.waiting_manual_merge)).count();
    CheckResult::ok(
        STATE,
        NAME,
        format!(
            "{} task(s) in review{}",
            review.len(),
            if manual > 0 { format!(", {manual} waiting for a manual merge") } else { String::new() }
        ),
    )
}

fn check_reviews(cfg: &Config, store: &Store) -> CheckResult {
    let tasks = match store.list_tasks() {
        Ok(t) => t,
        Err(e) => return CheckResult::fail(STATE, "reviews", format!("{e:#}"), "check the database"),
    };
    let errors = store.count_events_of_kind("review.error", Utc::now() - Duration::hours(24)).unwrap_or(0);
    review_status(&tasks, errors, cfg.scheduler.review_rounds_max)
}

fn check_dependencies(cfg: &Config, store: &Store) -> CheckResult {
    let tasks = match store.list_tasks() {
        Ok(t) => t,
        Err(e) => return CheckResult::fail(STATE, "dependencies", format!("{e:#}"), "check the database"),
    };
    let errors = store.count_events_of_kind("linear.parent_error", Utc::now() - Duration::hours(24)).unwrap_or(0);
    let closing = cfg.linear.manage_states && cfg.linear.done_state_parent.as_deref().is_some_and(|s| !s.trim().is_empty());
    dependency_status(&tasks, errors, closing)
}

/// Problems keeping task branches on a fresh base over the last day:
/// `stale_bases` branches created after `git fetch` failed
/// (`worktree.stale_base`), `ff_failures` failed fast-forwards of the local
/// default branch (`repo.fast_forward_failed`).
pub fn worktree_base_status(stale_bases: u64, ff_failures: u64) -> CheckResult {
    const NAME: &str = "worktree base";
    if stale_bases > 0 {
        return CheckResult::warn(
            STATE,
            NAME,
            format!("{stale_bases} branch(es) created in the last 24h after `git fetch` failed; they may lack merged work"),
            "run `git -C <repo.path> fetch origin` to see the error (credentials, network); see `powerqueue logs --events`, kind `worktree.stale_base`",
        );
    }
    if ff_failures > 0 {
        return CheckResult::warn(
            STATE,
            NAME,
            format!(
                "{ff_failures} failed fast-forward(s) of the local default branch in the last 24h (task branches are unaffected)"
            ),
            "see `powerqueue logs --events`, kind `repo.fast_forward_failed`; or set `repo.fast_forward_base = false`",
        );
    }
    CheckResult::ok(STATE, NAME, "new branches start from a freshly fetched base")
}

fn check_worktree_base(store: &Store) -> CheckResult {
    let since = Utc::now() - Duration::hours(24);
    let stale = store.count_events_of_kind("worktree.stale_base", since).unwrap_or(0);
    let ff = store.count_events_of_kind("repo.fast_forward_failed", since).unwrap_or(0);
    worktree_base_status(stale, ff)
}

/// The question relay through Linear comments: whether it is on
/// (`linear.post_comments` not `false`), which tasks wait for a reply to a
/// question posted on Linear (`waiting`), and failures typing replies or
/// hints into a session (`relay_errors`: `relay.error` events in the last day).
pub fn question_relay_status(
    mode: crate::config::PostComments,
    linear_enabled: bool,
    waiting: &[String],
    relay_errors: u64,
) -> CheckResult {
    const NAME: &str = "question relay";
    if !linear_enabled || !mode.questions() {
        return CheckResult::ok(
            STATE,
            NAME,
            "off (Linear disabled or `linear.post_comments = false`); answer questions with `task send` or `attach`",
        );
    }
    if relay_errors > 0 {
        return CheckResult::warn(
            STATE,
            NAME,
            format!("{relay_errors} Linear comment(s) could not be typed into a session in the last 24h"),
            "see `powerqueue logs --events`, kind `relay.error`; a lost reply can be sent with `powerqueue task send <key> \"...\"`",
        );
    }
    if waiting.is_empty() {
        CheckResult::ok(STATE, NAME, "on; no question waiting for a reply on Linear")
    } else {
        CheckResult::ok(
            STATE,
            NAME,
            format!("{} question(s) waiting for a reply on Linear: {}", waiting.len(), waiting.join(", ")),
        )
    }
}

fn check_question_relay(cfg: &Config, store: &Store) -> CheckResult {
    use crate::scheduler::relay::{RelayState, relay_key};
    let tasks = match store.list_open_tasks() {
        Ok(t) => t,
        Err(e) => return CheckResult::fail(STATE, "question relay", format!("{e:#}"), "check the database"),
    };
    let waiting: Vec<String> = tasks
        .iter()
        .filter(|t| matches!(t.state, TaskState::NeedsAttention | TaskState::InReview))
        .filter(|t| store.kv_get::<RelayState>(&relay_key(t.id)).ok().flatten().is_some_and(|r| r.open_question().is_some()))
        .map(|t| t.key.clone())
        .collect();
    let errors = store.count_events_of_kind("relay.error", Utc::now() - Duration::hours(24)).unwrap_or(0);
    question_relay_status(cfg.linear.post_comments, cfg.linear.enabled, &waiting, errors)
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
fn ledger_checks(cfg: &Config, ledger: &Ledger, readings: usize, typical_weighted: f64) -> Vec<CheckResult> {
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
    let source = ledger.window_fraction_source();
    if !ledger.has_window() {
        out.push(CheckResult::ok(
            ALGO,
            &name,
            format!("no rolling window (budget.providers.{p}.window_hours = 0 and the provider reports none)"),
        ));
    } else if window > MAX_WINDOW_FRACTION {
        out.push(CheckResult::warn(
            ALGO,
            &name,
            format!("{:.0}% of the {p} window used ({source})", window * 100.0),
            "lower scheduler.max_concurrent, or wait for the window to roll over",
        ));
    } else {
        out.push(CheckResult::ok(ALGO, &name, format!("{:.0}% of the {p} window used ({source})", window * 100.0)));
    }
    out.push(rate_check(ledger, readings));
    if let Some(top) = cfg.budget.provider(p).enabled_models().into_iter().next() {
        out.push(affordability_check(cfg, ledger, &top, typical_weighted));
    }
    out
}

/// Ratio between the configured and the learned period budget above which
/// `doctor` asks for the config to be updated.
pub const MAX_RATE_MISMATCH: f64 = 2.0;

/// Whether the exchange rate between our weighted tokens and the provider's
/// percentages is known, and whether the configured budget is far from it.
fn rate_check(ledger: &Ledger, readings: usize) -> CheckResult {
    let p = ledger.provider;
    let name = format!("{p} usage rate");
    let key = format!("budget.providers.{p}.period_weighted_tokens");
    match ledger.learned.period {
        Some(r) => {
            let configured = ledger.configured_period_budget.max(1.0);
            let ratio = (r.budget / configured).max(configured / r.budget);
            let detail = format!(
                "100% of the period ≈ {} weighted tokens, learned from {} readings ({:.1} points over {}); {key} = {}",
                human_f64(r.budget),
                r.samples,
                r.observed_delta * 100.0,
                crate::cli::output::human_duration((r.to - r.from).num_seconds()),
                human_f64(configured)
            );
            if ratio > MAX_RATE_MISMATCH {
                CheckResult::warn(
                    ALGO,
                    &name,
                    format!("{detail}: the configured budget is {ratio:.1}× off"),
                    format!("set {key} = {:.0} so pacing is right before the period's first readings arrive", r.budget.round()),
                )
            } else {
                CheckResult::ok(ALGO, &name, detail)
            }
        }
        None if ledger.observed.is_some() => CheckResult::ok(
            ALGO,
            &name,
            format!(
                "not learned yet ({readings} reading(s) this period; needs two at least {:.0} points and {} weighted tokens apart); pacing starts from the provider's latest reading and counts new usage at {key} = {}",
                crate::budget::MIN_OBSERVED_DELTA * 100.0,
                human_f64(crate::budget::MIN_MEASURED_DELTA),
                human_f64(ledger.configured_period_budget)
            ),
        ),
        None => CheckResult::ok(
            ALGO,
            &name,
            format!(
                "no reading from {p} yet; pacing uses {key} = {} as the whole allowance",
                human_f64(ledger.configured_period_budget)
            ),
        ),
    }
}

/// Whether the provider's most capable model can hold a typical task at all:
/// a share that is smaller than one task (after the model's cost weight)
/// means the model can never lend itself to less critical work.
fn affordability_check(cfg: &Config, ledger: &Ledger, top: &ModelTier, typical_weighted: f64) -> CheckResult {
    let p = ledger.provider;
    let name = format!("{} share", top.alias());
    let weight = tier_weight(&cfg.budget, top);
    let cost = typical_weighted * weight;
    let share = ledger.tier(top).period_budget;
    let key = format!("budget.providers.{p}.models.{}", top.alias());
    if share <= 0.0 {
        return CheckResult::ok(ALGO, &name, format!("{top} has no share; only `task model {top}` overrides use it"));
    }
    if cost > share * (1.0 - cfg.budget.safety_margin) {
        CheckResult::warn(
            ALGO,
            &name,
            format!(
                "{top}'s share ({}) is smaller than one typical task ({} × weight {weight} = {}): it can never lend itself to less critical work",
                human_f64(share),
                human_f64(typical_weighted),
                human_f64(cost)
            ),
            format!("raise {key}.share or lower {key}.weight (work {top} is reserved for is not capped by the share)"),
        )
    } else {
        CheckResult::ok(
            ALGO,
            &name,
            format!("{top}'s share ({}) holds about {:.0} typical tasks", human_f64(share), share / cost.max(1.0)),
        )
    }
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
    // A typical task: what the estimator predicts with no history-specific
    // features (the global median, or the default guess).
    let typical = Estimator::from_summaries(&store.task_usage_summaries().unwrap_or_default())
        .predict(&crate::domain::Task::new("doctor", "typical", crate::domain::TaskSource::Manual))
        .weighted_tokens;
    ledgers
        .ordered(&cfg.budget.provider_order)
        .into_iter()
        .flat_map(|l| {
            let readings =
                load_observations(store, l.provider).map(|h| h.iter().filter(|s| l.period.contains(s.at)).count()).unwrap_or(0);
            ledger_checks(cfg, l, readings, typical)
        })
        .collect()
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
    results.push(check_gh(cfg, store));
    results.extend(check_claude(cfg));
    results.push(check_terminal());

    results.push(check_config(cfg, paths));
    results.push(check_repo(cfg));
    results.push(check_worktree_root(cfg, paths));
    results.push(check_priority_file(cfg, paths));
    results.push(check_repo_overrides(cfg));
    results.push(check_prompt_template(cfg));
    results.push(check_tune_drafts(paths));
    results.extend(check_provider_models(cfg));
    results.extend(check_period_anchors(cfg, store));
    results.extend(check_providers(cfg));
    results.push(check_workspace_trust(cfg, fix));
    if let Some(r) = check_session_inbox(cfg, paths) {
        results.push(r);
    }

    results.extend(check_secrets(cfg, secrets, online).await);
    if cfg.github.enabled {
        let since = Utc::now() - Duration::days(1);
        let errors = store.count_events_of_kind("github.error", since)?;
        let missed = store.count_events_of_kind("github.update_failed", since)?
            + store.count_events_of_kind("github.update_skipped", since)?;
        results.push(if errors + missed == 0 {
            CheckResult::ok(STATE, "GitHub sync", "no API failures or missed lifecycle updates in 24 h")
        } else {
            CheckResult::warn(STATE, "GitHub sync", format!("{errors} API error(s), {missed} missed lifecycle update(s) in 24 h"),
                "inspect github.* events in logs/task show; verify token Issues permissions and repository access, wait for rate-limit reset, and manually reconcile missed labels/comments/closure")
        });
    }

    results.push(check_db(store));
    results.push(check_daemon(store));
    results.push(check_service(cfg));
    results.push(check_scheduling_pause(store));
    results.push(check_stuck_tasks(cfg, store, fix));
    results.push(check_dependencies(cfg, store));
    results.push(check_reviews(cfg, store));
    results.push(check_worktree_base(store));
    results.push(check_question_relay(cfg, store));
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
    fn rate_and_share_checks_explain_what_blocks_fable() {
        use crate::budget::{ObservedUsage, save_observed};
        use crate::domain::{Task, TaskSource, TokenUsage, UsageRecord};
        let store = Store::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.budget.providers.claude.period_anchor = Some("2026-09-28T00:00:00Z".into());
        cfg.budget.providers.claude.period_weighted_tokens = 80_000_000;
        let now = Utc::now();
        let task = Task::new("ENG-1", "t", TaskSource::Manual);
        store.insert_task(&task).unwrap();
        // Two readings 5 points apart with 50M weighted tokens between them: 1B per period.
        let first = ObservedUsage { period_used: Some(0.30), ..ObservedUsage::empty(now - Duration::hours(3)) };
        save_observed(&store, Provider::Claude, &first).unwrap();
        let rec = UsageRecord {
            session_id: uuid::Uuid::new_v4(),
            task_id: task.id,
            message_id: "m1".into(),
            model_id: "claude-sonnet-5".into(),
            tier: ModelTier::sonnet(),
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 10_000_000,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
            timestamp: now - Duration::hours(2),
        };
        assert!(store.record_usage(&rec).unwrap());
        let second = ObservedUsage { period_used: Some(0.35), ..ObservedUsage::empty(now - Duration::hours(1)) };
        save_observed(&store, Provider::Claude, &second).unwrap();
        let results = check_ledger(&cfg, &store);
        let rate = results.iter().find(|r| r.name == "claude usage rate").unwrap();
        assert_eq!(rate.status, Status::Warn, "{}", rate.detail);
        assert!(rate.detail.contains("1.0B weighted tokens") && rate.detail.contains("12.5× off"), "{}", rate.detail);
        let hint = rate.fix_hint.clone().unwrap_or_default();
        assert!(hint.contains("period_weighted_tokens = 1000000000"), "{hint}");

        // A fable share too small for one typical task.
        cfg.budget.providers.claude.models.get_mut(&ModelTier::fable()).unwrap().share = 0.001;
        let results = check_ledger(&cfg, &store);
        let share = results.iter().find(|r| r.name == "fable share").unwrap();
        assert_eq!(share.status, Status::Warn, "{}", share.detail);
        assert!(share.detail.contains("smaller than one typical task"), "{}", share.detail);
        let hint = share.fix_hint.clone().unwrap_or_default();
        assert!(hint.contains("models.fable.share"), "{hint}");
    }

    #[test]
    fn ledger_checks_per_provider() {
        let store = Store::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.budget.providers.codex.enabled = true;
        cfg.budget.providers.codex.window_hours = 0;
        let results = check_ledger(&cfg, &store);
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "fable reservation",
                "window pressure",
                "claude usage rate",
                "fable share",
                "gpt-6.1-sol reservation",
                "codex window pressure",
                "codex usage rate",
                "gpt-6.1-sol share"
            ]
        );
        assert!(results[5].detail.contains("no rolling window"), "{}", results[5].detail);
        assert!(results[2].detail.contains("no reading from claude yet"), "{}", results[2].detail);
        assert_eq!(results[3].status, Status::Ok, "{}", results[3].detail);
        assert!(results[3].detail.contains("holds about 8 typical tasks"), "{}", results[3].detail);
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
    fn worktree_base_status_warns_on_recent_fetch_and_fast_forward_failures() {
        assert_eq!(worktree_base_status(0, 0).status, Status::Ok);
        let r = worktree_base_status(2, 1);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("2 branch(es)"), "{}", r.detail);
        let r = worktree_base_status(0, 3);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("3 failed fast-forward"), "{}", r.detail);
    }

    #[test]
    fn question_relay_status_reports_mode_waiting_and_errors() {
        use crate::config::PostComments;
        let r = question_relay_status(PostComments::Off, true, &[], 3);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.starts_with("off"), "{}", r.detail);
        let r = question_relay_status(PostComments::Questions, true, &["ENG-1".into()], 0);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("1 question(s) waiting for a reply on Linear: ENG-1"), "{}", r.detail);
        let r = question_relay_status(PostComments::All, true, &[], 2);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("2 Linear comment(s)"), "{}", r.detail);
    }

    #[test]
    fn review_status_reports_parked_and_failing_watchers() {
        let mut watched = Task::new("R-1", "R-1", crate::domain::TaskSource::Manual);
        watched.state = TaskState::InReview;
        watched.pr_url = Some("https://github.com/o/r/pull/1".into());
        watched.review = Some(crate::domain::ReviewWatch::armed(Utc::now(), None));
        let r = review_status(std::slice::from_ref(&watched), 0, 5);
        assert_eq!((r.status, r.detail.as_str()), (Status::Ok, "1 task(s) in review"));
        assert_eq!(review_status(std::slice::from_ref(&watched), 3, 5).status, Status::Warn);
        let mut parked = watched.clone();
        parked.key = "R-2".into();
        parked.state = TaskState::NeedsAttention;
        parked.last_error = Some("PR #1 has not changed".into());
        let r = review_status(&[watched, parked], 0, 5);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("R-2 (PR #1 has not changed)"), "{}", r.detail);
        assert_eq!(review_status(&[], 0, 5).status, Status::Ok);
    }

    #[test]
    fn dependency_check_reports_blocked_cycles_and_parent_errors() {
        use crate::domain::{LinkedIssue, TaskSource};
        let linked = |key: &str, state_type: &str| LinkedIssue {
            key: key.into(),
            title: String::new(),
            state_type: state_type.into(),
            pr_merged: false,
        };
        let task = |key: &str, state: TaskState, blockers: &[&str]| {
            let mut t = Task::new(key, key, TaskSource::Manual);
            t.state = state;
            t.blocked_by = blockers.iter().map(|b| linked(b, "started")).collect();
            t
        };
        let r = dependency_status(&[task("A", TaskState::Queued, &[])], 0, true);
        assert_eq!((r.status, r.detail.as_str()), (Status::Ok, "no task waits on another issue"));

        let r = dependency_status(&[task("A", TaskState::Blocked, &["B"])], 0, true);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("A ← B"), "{}", r.detail);

        let tasks = [
            task("A", TaskState::Blocked, &["B"]),
            task("B", TaskState::Blocked, &["C"]),
            task("C", TaskState::Blocked, &["A"]),
            task("D", TaskState::Blocked, &["A"]),
        ];
        let r = dependency_status(&tasks, 0, true);
        assert_eq!(r.status, Status::Fail);
        assert_eq!(r.detail.matches('→').count(), 3, "one cycle reported once: {}", r.detail);
        let mut done = tasks.clone();
        done[2].state = TaskState::Completed;
        assert_ne!(dependency_status(&done, 0, true).status, Status::Fail, "a terminal task breaks the cycle");

        let r = dependency_status(&[task("A", TaskState::Queued, &[])], 2, true);
        assert_eq!(r.status, Status::Warn);
        assert!(r.fix_hint.unwrap().contains("done_state_parent"));

        let mut parent = task("P", TaskState::Blocked, &[]);
        parent.children = vec![linked("C-1", "started")];
        assert_eq!(dependency_status(std::slice::from_ref(&parent), 0, false).status, Status::Warn);
        let r = dependency_status(std::slice::from_ref(&parent), 0, true);
        assert!(r.detail.contains("P (parent; open: C-1)"), "{}", r.detail);
        parent.children[0].state_type = "canceled".into();
        let r = dependency_status(&[parent], 0, true);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("all canceled") && r.detail.contains('P'), "{}", r.detail);
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

    #[test]
    fn tune_draft_checks() {
        use crate::cli::TuneScope;
        use crate::tune::{Draft, DraftStatus};
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        assert_eq!(check_tune_drafts(&paths).status, Status::Ok, "no tune/ directory yet");
        let mk = |id: &str, status: DraftStatus| {
            let mut d = Draft::create(
                &paths,
                id,
                "make INC-1 critical and urgent",
                TuneScope::All,
                "sonnet",
                &paths.priority_file(),
                None,
                &paths.config_file(),
                "[repo]\npath = \"/r\"\n",
            )
            .unwrap();
            d.set_status(status, vec!["timed out".into()]).unwrap();
            d
        };
        mk("20260101T000000Z-a00000", DraftStatus::Applied);
        assert_eq!(check_tune_drafts(&paths).status, Status::Ok);
        mk("20260101T000001Z-a00001", DraftStatus::Failed);
        let r = check_tune_drafts(&paths);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("timed out"), "{r:?}");
        mk("20260101T000002Z-a00002", DraftStatus::Proposed);
        let r = check_tune_drafts(&paths);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("1 proposal(s) not applied"), "{r:?}");
        assert!(r.fix_hint.as_deref().unwrap_or("").contains("tune --apply"));
    }

    #[test]
    fn prompt_template_check_covers_missing_unknown_and_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        assert_eq!(check_prompt_template(&cfg).status, Status::Ok);
        let tpl = dir.path().join("prompt.md");
        cfg.prompt.template = Some(tpl.to_string_lossy().to_string());
        let r = check_prompt_template(&cfg);
        assert_eq!(r.status, Status::Fail);
        assert!(r.detail.contains("cannot read"), "{r:?}");
        std::fs::write(&tpl, "# {{key}}\n{{bogus}}\n{{completion_protocol}}\n").unwrap();
        let r = check_prompt_template(&cfg);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("bogus"), "{r:?}");
        std::fs::write(&tpl, "# {{key}}\n").unwrap();
        let r = check_prompt_template(&cfg);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("completion_protocol"), "{r:?}");
        std::fs::write(&tpl, "{{default_prompt}}\n").unwrap();
        assert_eq!(check_prompt_template(&cfg).status, Status::Ok);
    }
}
