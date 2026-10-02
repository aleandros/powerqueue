//! `powerqueue init` — guided first-time setup.
//!
//! Interactive by default (dialoguer prompts, indicatif spinners for network
//! calls); `--non-interactive` (or a non-TTY stdin) uses flags, environment
//! variables and defaults and fails with a list of what is missing.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, Datelike, Local, NaiveTime, TimeZone, Utc, Weekday};
use dialoguer::theme::{ColorfulTheme, SimpleTheme, Theme};
use dialoguer::{Confirm, Input, MultiSelect, Password, Select};
use indicatif::ProgressBar;
use owo_colors::{OwoColorize, Stream, Style};

use crate::cli::{Context, InitArgs};
use crate::config::{BudgetConfig, Config, config_template, write_private};
use crate::doctor::claude_auth_status;
use crate::jev::JevClient;
use crate::linear::client::{LinearClient, WorkflowState};
use crate::secrets::{SecretKind, SecretOrigin};
use crate::store::Store;
use crate::tmux::Tmux;
use crate::worktree::Repo;

/// Everything `init` decides before writing files.
#[derive(Debug, Clone)]
pub struct Answers {
    pub repo_path: PathBuf,
    pub default_branch: Option<String>,
    pub linear_enabled: bool,
    pub team_keys: Vec<String>,
    pub queued_states: Vec<String>,
    pub in_progress_state: Option<String>,
    pub done_state: Option<String>,
    pub jev_enabled: bool,
    pub max_concurrent: u32,
    pub permission_mode: String,
    pub period_weighted_tokens: u64,
    pub period_anchor: Option<DateTime<Utc>>,
}

impl Default for Answers {
    fn default() -> Self {
        let budget = BudgetConfig::default();
        let linear = crate::config::LinearConfig::default();
        Self {
            repo_path: PathBuf::new(),
            default_branch: None,
            linear_enabled: true,
            team_keys: Vec::new(),
            queued_states: linear.queued_states,
            in_progress_state: linear.in_progress_state,
            done_state: linear.done_state,
            jev_enabled: false,
            max_concurrent: crate::config::SchedulerConfig::default().max_concurrent,
            permission_mode: crate::config::ClaudeConfig::default().permission_mode,
            period_weighted_tokens: budget.period_weighted_tokens,
            period_anchor: None,
        }
    }
}

/// Permission modes offered by the setup, most recommended first.
pub const PERMISSION_MODES: [(&str, &str); 3] = [
    ("acceptEdits", "auto-accept file edits, ask for other tools (recommended)"),
    ("bypassPermissions", "fully unattended; Claude may run anything"),
    ("default", "ask for everything (sessions will stall without a human)"),
];

/// Pick sensible queued / in-progress / done states from a team's workflow:
/// queued = the `unstarted` state called "Todo" (else the first unstarted,
/// else the first backlog), in-progress = the first `started` state
/// (preferring "In Progress"), done = "In Review" if present, else the first
/// `completed` state.
pub fn pick_default_states(states: &[WorkflowState]) -> (Vec<String>, Option<String>, Option<String>) {
    let by_kind = |kind: &'static str| states.iter().filter(move |s| s.kind.eq_ignore_ascii_case(kind));
    let named = |name: &str| states.iter().find(|s| s.name.eq_ignore_ascii_case(name));

    let queued = by_kind("unstarted")
        .find(|s| s.name.eq_ignore_ascii_case("todo"))
        .or_else(|| by_kind("unstarted").next())
        .or_else(|| by_kind("backlog").next())
        .map(|s| vec![s.name.clone()])
        .unwrap_or_default();
    let in_progress = by_kind("started")
        .find(|s| s.name.eq_ignore_ascii_case("in progress"))
        .or_else(|| by_kind("started").next())
        .map(|s| s.name.clone());
    let done = named("In Review").or_else(|| by_kind("completed").next()).map(|s| s.name.clone());
    (queued, in_progress, done)
}

/// Parse the period reset instant as typed by the user. Accepted forms:
/// RFC 3339 (`2026-03-02T09:00:00Z`), `in 3d4h`, a weekday + time such as
/// `Mon 00:00` / `Monday 9:00am` (the most recent such instant, local time),
/// or a bare time (`09:00`, today). An empty string is an error.
pub fn parse_reset_anchor(input: &str) -> Result<DateTime<Utc>> {
    parse_reset_anchor_at(input, Local::now())
}

/// [`parse_reset_anchor`] with an explicit "now" (local time) for tests.
pub fn parse_reset_anchor_at(input: &str, now: DateTime<Local>) -> Result<DateTime<Utc>> {
    let text = input.trim();
    if text.is_empty() {
        bail!("empty reset time");
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Some(rest) = text.strip_prefix("in ") {
        let d = humantime::parse_duration(rest.trim()).map_err(|e| anyhow!("cannot parse duration `{rest}`: {e}"))?;
        return Ok(now.with_timezone(&Utc) + chrono::Duration::from_std(d)?);
    }
    let mut parts = text.split_whitespace();
    let first = parts.next().unwrap_or_default();
    let (weekday, time_text) = match first.parse::<Weekday>() {
        Ok(wd) => (Some(wd), parts.collect::<Vec<_>>().join("")),
        Err(_) => (None, text.replace(' ', "")),
    };
    let time = parse_time(&time_text)
        .ok_or_else(|| anyhow!("cannot parse `{input}`: use RFC 3339, `in 3d4h`, `Mon 00:00`, `Monday 9:00am` or `09:00`"))?;
    let today = now.date_naive();
    let mut date = today;
    if let Some(wd) = weekday {
        let back = (today.weekday().num_days_from_monday() + 7 - wd.num_days_from_monday()) % 7;
        date = today - chrono::Duration::days(i64::from(back));
        if back == 0 && time > now.time() {
            date -= chrono::Duration::days(7);
        }
    }
    let naive = date.and_time(time);
    let local =
        Local.from_local_datetime(&naive).earliest().ok_or_else(|| anyhow!("`{input}` does not exist in the local timezone"))?;
    Ok(local.with_timezone(&Utc))
}

/// `9`, `09:00`, `9:30pm`, `12am`, `21:15:00`.
fn parse_time(s: &str) -> Option<NaiveTime> {
    let lower = s.trim().to_ascii_lowercase();
    let (body, pm) = if let Some(b) = lower.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = lower.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (lower.as_str(), None)
    };
    let mut parts = body.split(':');
    let hour: u32 = parts.next()?.trim().parse().ok()?;
    let minute: u32 = parts.next().map(|m| m.trim().parse().ok()).unwrap_or(Some(0))?;
    let second: u32 = parts.next().map(|m| m.trim().parse().ok()).unwrap_or(Some(0))?;
    if parts.next().is_some() {
        return None;
    }
    let hour = match pm {
        Some(_) if hour == 0 || hour > 12 => return None,
        Some(true) if hour < 12 => hour + 12,
        Some(false) if hour == 12 => 0,
        _ => hour,
    };
    NaiveTime::from_hms_opt(hour, minute, second)
}

/// Items required by `--non-interactive` that were not provided.
pub fn missing_for_non_interactive(args: &InitArgs, repo_is_git: bool) -> Vec<String> {
    let mut missing = Vec::new();
    if !repo_is_git {
        missing.push("a git repository (--repo PATH or run inside one)".to_string());
    }
    if !args.no_linear && args.linear_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).is_none() {
        missing.push("Linear API key (--linear-key, LINEAR_API_KEY, or --no-linear)".to_string());
    }
    missing
}

/// Compose `config.toml` from the answers: the commented template header plus
/// the answers applied to a parsed default config.
pub fn render_config(answers: &Answers) -> Result<String> {
    let template = config_template(&answers.repo_path.display().to_string(), &answers.team_keys);
    let header: Vec<&str> = template.lines().take_while(|l| l.starts_with('#')).collect();
    let mut cfg = Config::from_toml(&template)?;
    cfg.linear.enabled = answers.linear_enabled;
    cfg.linear.team_keys = answers.team_keys.clone();
    if !answers.queued_states.is_empty() {
        cfg.linear.queued_states = answers.queued_states.clone();
    }
    cfg.linear.in_progress_state = answers.in_progress_state.clone();
    cfg.linear.done_state = answers.done_state.clone();
    cfg.priority.jev.enabled = answers.jev_enabled;
    cfg.scheduler.max_concurrent = answers.max_concurrent;
    cfg.claude.permission_mode = answers.permission_mode.clone();
    cfg.budget.period_weighted_tokens = answers.period_weighted_tokens;
    cfg.budget.period_anchor = answers.period_anchor.map(|a| a.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    cfg.ensure_valid()?;
    Ok(format!("{}\n\n{}", header.join("\n"), cfg.to_toml()?))
}

/// Write config, PRIORITY.md (if absent), directories and the database.
pub fn write_files(ctx: &Context, answers: &Answers) -> Result<Vec<String>> {
    let paths = &ctx.paths;
    paths.ensure()?;
    let mut written = Vec::new();
    let config_file = paths.config_file();
    write_private(&config_file, render_config(answers)?.as_bytes())
        .with_context(|| format!("write {}", config_file.display()))?;
    written.push(config_file.display().to_string());
    let priority_file = paths.priority_file();
    if !priority_file.exists() {
        std::fs::write(&priority_file, crate::priority::template())
            .with_context(|| format!("write {}", priority_file.display()))?;
        written.push(priority_file.display().to_string());
    }
    let db = paths.database();
    Store::open(&db)?;
    written.push(db.display().to_string());
    Ok(written)
}

struct Ui {
    theme: Box<dyn Theme>,
    color: bool,
}

impl Ui {
    fn new(color: bool) -> Self {
        let theme: Box<dyn Theme> = if color { Box::new(ColorfulTheme::default()) } else { Box::new(SimpleTheme) };
        Self { theme, color }
    }
    fn theme(&self) -> &dyn Theme {
        self.theme.as_ref()
    }
    fn section(&self, title: &str) {
        println!();
        println!(
            "{}",
            if self.color {
                title.if_supports_color(Stream::Stdout, |t| t.style(Style::new().bold().underline())).to_string()
            } else {
                title.to_string()
            }
        );
    }
    fn ok(&self, msg: &str) {
        println!(
            "  {} {msg}",
            if self.color { "✓".if_supports_color(Stream::Stdout, |t| t.green()).to_string() } else { "ok".to_string() }
        );
    }
    fn warn(&self, msg: &str) {
        println!(
            "  {} {msg}",
            if self.color {
                "!".if_supports_color(Stream::Stdout, |t| t.style(Style::new().yellow().bold())).to_string()
            } else {
                "warning:".to_string()
            }
        );
    }
    fn fail(&self, msg: &str) {
        println!(
            "  {} {msg}",
            if self.color {
                "✗".if_supports_color(Stream::Stdout, |t| t.style(Style::new().red().bold())).to_string()
            } else {
                "error:".to_string()
            }
        );
    }
    fn spinner(&self, msg: &str) -> ProgressBar {
        let pb = ProgressBar::new_spinner().with_message(msg.to_string());
        pb.enable_steady_tick(Duration::from_millis(80));
        pb
    }
}

fn detect_repo(args_repo: Option<&Path>) -> Result<PathBuf> {
    let raw = match args_repo {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("determine current directory")?,
    };
    Ok(raw.canonicalize().unwrap_or(raw))
}

fn repo_step(ui: &Ui, args: &InitArgs, interactive: bool) -> Result<(PathBuf, Option<String>)> {
    ui.section("Repository");
    let mut path = detect_repo(args.repo.as_deref())?;
    loop {
        let repo = Repo::new(&path);
        if repo.is_repo() {
            let branch = repo.default_branch().ok();
            ui.ok(&format!("{} (default branch: {})", path.display(), branch.as_deref().unwrap_or("unknown")));
            return Ok((path, branch));
        }
        ui.fail(&format!("{} is not a git repository", path.display()));
        if !interactive {
            bail!("{} is not a git repository (use --repo PATH)", path.display());
        }
        let typed: String = Input::with_theme(ui.theme()).with_prompt("Path to the repository").interact_text()?;
        path = detect_repo(Some(Path::new(typed.trim())))?;
    }
}

fn linear_step(
    ctx: &mut Context,
    ui: &Ui,
    args: &InitArgs,
    interactive: bool,
    rt: &tokio::runtime::Runtime,
) -> Result<Option<LinearClient>> {
    ui.section("Linear");
    if args.no_linear {
        println!("  skipped (--no-linear); manual tasks only");
        return Ok(None);
    }
    let endpoint = crate::config::LinearConfig::default().endpoint;
    let mut candidate = args.linear_key.clone().map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    let from_env =
        candidate.is_some() && std::env::var(SecretKind::LinearApiKey.env_var()).ok().as_deref() == candidate.as_deref();
    if candidate.is_none() && !interactive {
        bail!("Linear API key missing (--linear-key or LINEAR_API_KEY)");
    }
    loop {
        let key = match candidate.take() {
            Some(k) => k,
            None => {
                let typed = Password::with_theme(ui.theme())
                    .with_prompt("Linear API key (https://linear.app/settings/account/security; empty to skip Linear)")
                    .allow_empty_password(true)
                    .interact()?;
                if typed.trim().is_empty() {
                    let skip = Confirm::with_theme(ui.theme())
                        .with_prompt("Skip Linear and use manual tasks only?")
                        .default(false)
                        .interact()?;
                    if skip {
                        ui.warn("Linear disabled; add work with `powerqueue add`");
                        return Ok(None);
                    }
                    continue;
                }
                typed.trim().to_string()
            }
        };
        let client = LinearClient::new(endpoint.clone(), key.clone())?;
        let pb = ui.spinner("checking the Linear API key…");
        let viewer = rt.block_on(client.viewer());
        pb.finish_and_clear();
        match viewer {
            Ok(v) => {
                ui.ok(&format!("authenticated as {} <{}>", v.name, v.email));
                let secrets = ctx.secrets();
                if from_env {
                    ui.ok(&format!("using {} from the environment (not stored)", SecretKind::LinearApiKey.env_var()));
                } else {
                    secrets.set(SecretKind::LinearApiKey, &key)?;
                    ui.ok(&format!("key stored in {}", secrets.backend_description()));
                    if secrets.get_with_origin(SecretKind::LinearApiKey)?.map(|(_, o)| o) == Some(SecretOrigin::File) {
                        ui.warn("no OS keychain available: the key is in a 0600 file in the config directory. Keep that directory private.");
                    }
                }
                return Ok(Some(client));
            }
            Err(e) => {
                ui.fail(&format!("Linear rejected the key: {e:#}"));
                if !interactive {
                    return Err(e.context("Linear API key check failed"));
                }
                let retry = Confirm::with_theme(ui.theme()).with_prompt("Try another key?").default(true).interact()?;
                if !retry {
                    bail!("Linear API key check failed");
                }
            }
        }
    }
}

fn teams_step(
    ui: &Ui,
    client: &LinearClient,
    args: &InitArgs,
    interactive: bool,
    answers: &mut Answers,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    ui.section("Teams and workflow states");
    let pb = ui.spinner("fetching teams…");
    let teams = rt.block_on(client.teams());
    pb.finish_and_clear();
    let teams = match teams {
        Ok(t) => t,
        Err(e) => {
            ui.warn(&format!("could not list teams ({e:#}); using --team values as given"));
            answers.team_keys = args.team.clone();
            return Ok(());
        }
    };
    if teams.is_empty() {
        ui.warn("the API key sees no teams; `linear.team_keys` left empty (all teams)");
        return Ok(());
    }
    let preselected: Vec<bool> = teams.iter().map(|t| args.team.iter().any(|k| k.eq_ignore_ascii_case(&t.key))).collect();
    let chosen: Vec<usize> = if interactive {
        let labels: Vec<String> = teams.iter().map(|t| format!("{} — {}", t.key, t.name)).collect();
        let defaults = if preselected.iter().any(|p| *p) { preselected.clone() } else { vec![teams.len() == 1; teams.len()] };
        MultiSelect::with_theme(ui.theme())
            .with_prompt("Teams to pull issues from (space to toggle, enter to confirm)")
            .items(&labels)
            .defaults(&defaults)
            .interact()?
    } else {
        preselected.iter().enumerate().filter(|(_, p)| **p).map(|(i, _)| i).collect()
    };
    answers.team_keys = chosen.iter().map(|i| teams[*i].key.clone()).collect();
    if answers.team_keys.is_empty() {
        ui.ok("no team selected: issues from every visible team are considered");
    } else {
        ui.ok(&format!("teams: {}", answers.team_keys.join(", ")));
    }

    let Some(first) = answers.team_keys.first().cloned().or_else(|| teams.first().map(|t| t.key.clone())) else { return Ok(()) };
    let pb = ui.spinner(&format!("fetching workflow states of {first}…"));
    let states = rt.block_on(client.workflow_states(&first));
    pb.finish_and_clear();
    let states = match states {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            ui.warn(&format!("{first} has no workflow states; keeping defaults"));
            return Ok(());
        }
        Err(e) => {
            ui.warn(&format!("could not fetch workflow states ({e:#}); keeping defaults"));
            return Ok(());
        }
    };
    let (queued, in_progress, done) = pick_default_states(&states);
    if !interactive {
        if !queued.is_empty() {
            answers.queued_states = queued;
        }
        answers.in_progress_state = in_progress.or(answers.in_progress_state.take());
        answers.done_state = done.or(answers.done_state.take());
        ui.ok(&format!(
            "states: queued {:?}, in progress {:?}, done {:?}",
            answers.queued_states, answers.in_progress_state, answers.done_state
        ));
        return Ok(());
    }
    let labels: Vec<String> = states.iter().map(|s| format!("{} ({})", s.name, s.kind)).collect();
    let defaults: Vec<bool> = states.iter().map(|s| queued.contains(&s.name)).collect();
    let picked = MultiSelect::with_theme(ui.theme())
        .with_prompt("States that mean \"ready for the agent\"")
        .items(&labels)
        .defaults(&defaults)
        .interact()?;
    if !picked.is_empty() {
        answers.queued_states = picked.iter().map(|i| states[*i].name.clone()).collect();
    }
    let mut options: Vec<String> = vec!["(do not change the state)".to_string()];
    options.extend(labels.iter().cloned());
    let idx_of = |name: Option<&String>| name.and_then(|n| states.iter().position(|s| &s.name == n)).map(|i| i + 1).unwrap_or(0);
    let ip = Select::with_theme(ui.theme())
        .with_prompt("State to set when a session starts")
        .items(&options)
        .default(idx_of(in_progress.as_ref()))
        .interact()?;
    answers.in_progress_state = (ip > 0).then(|| states[ip - 1].name.clone());
    let dn = Select::with_theme(ui.theme())
        .with_prompt("State to set when the task completes")
        .items(&options)
        .default(idx_of(done.as_ref()))
        .interact()?;
    answers.done_state = (dn > 0).then(|| states[dn - 1].name.clone());
    Ok(())
}

fn tools_step(ui: &Ui, claude_binary: &str, tmux_binary: &str) {
    ui.section("Local tools");
    match std::process::Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => ui.ok(String::from_utf8_lossy(&o.stdout).trim()),
        _ => ui.fail("git not found on PATH"),
    }
    match Tmux::new(tmux_binary, None).version() {
        Ok(v) => ui.ok(&v),
        Err(e) => ui.fail(&format!("tmux not usable: {e:#} (install tmux; it hosts the Claude sessions)")),
    }
    match claude_auth_status(claude_binary) {
        Ok(auth) if auth.logged_in => {
            ui.ok(&format!("claude is logged in{}", auth.method.as_deref().map(|m| format!(" ({m})")).unwrap_or_default()))
        }
        Ok(auth) => ui.warn(&format!("claude is not logged in ({}); run `{claude_binary} auth login`", auth.detail)),
        Err(e) => ui.fail(&format!("`{claude_binary} auth status` failed: {e:#}")),
    }
}

fn jev_step(ctx: &mut Context, ui: &Ui, args: &InitArgs, interactive: bool, rt: &tokio::runtime::Runtime) -> Result<bool> {
    ui.section("Jev (optional ticket scoring by TypeSafe)");
    let mut candidate = args.jev_key.clone().map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    let from_env = candidate.is_some() && std::env::var(SecretKind::JevApiKey.env_var()).ok().as_deref() == candidate.as_deref();
    loop {
        let key = match candidate.take() {
            Some(k) => k,
            None if interactive => {
                let typed = Password::with_theme(ui.theme())
                    .with_prompt("Jev API key (empty to skip)")
                    .allow_empty_password(true)
                    .interact()?;
                if typed.trim().is_empty() {
                    ui.ok("skipped; enable later with `powerqueue secrets set jev`");
                    return Ok(false);
                }
                typed.trim().to_string()
            }
            None => {
                ui.ok("skipped");
                return Ok(false);
            }
        };
        let jev = crate::config::JevConfig::default();
        let client = JevClient::new(jev.endpoint, key.clone(), jev.model)?;
        let pb = ui.spinner("checking the Jev API key…");
        let ping = rt.block_on(client.ping());
        pb.finish_and_clear();
        match ping {
            Ok(()) => {
                if from_env {
                    ui.ok(&format!("using {} from the environment", SecretKind::JevApiKey.env_var()));
                } else {
                    let secrets = ctx.secrets();
                    secrets.set(SecretKind::JevApiKey, &key)?;
                    ui.ok(&format!("key stored in {}", secrets.backend_description()));
                }
                return Ok(true);
            }
            Err(e) => {
                ui.fail(&format!("Jev rejected the key: {e:#}"));
                if !interactive {
                    return Err(e.context("Jev API key check failed"));
                }
                if !Confirm::with_theme(ui.theme()).with_prompt("Try another key?").default(false).interact()? {
                    return Ok(false);
                }
            }
        }
    }
}

fn tuning_step(ui: &Ui, interactive: bool, answers: &mut Answers) -> Result<()> {
    ui.section("Scheduling and budget");
    if !interactive {
        ui.ok(&format!(
            "defaults: {} concurrent sessions, permission mode {}, {} weighted tokens per week",
            answers.max_concurrent, answers.permission_mode, answers.period_weighted_tokens
        ));
        return Ok(());
    }
    answers.max_concurrent = Input::with_theme(ui.theme())
        .with_prompt("How many Claude sessions may run at once?")
        .default(answers.max_concurrent)
        .validate_with(|v: &u32| if *v >= 1 { Ok(()) } else { Err("must be at least 1") })
        .interact_text()?;
    let labels: Vec<String> = PERMISSION_MODES.iter().map(|(m, d)| format!("{m} — {d}")).collect();
    let mode = Select::with_theme(ui.theme()).with_prompt("Permission mode for sessions").items(&labels).default(0).interact()?;
    answers.permission_mode = PERMISSION_MODES[mode].0.to_string();
    answers.period_weighted_tokens = Input::with_theme(ui.theme())
        .with_prompt("Weekly budget in weighted tokens (see docs/budget.md)")
        .default(answers.period_weighted_tokens)
        .interact_text()?;
    loop {
        let typed: String = Input::with_theme(ui.theme())
            .with_prompt(
                "When does your weekly usage reset? (as shown by /usage, e.g. 'Mon 00:00' or RFC 3339; empty to set later)",
            )
            .allow_empty(true)
            .interact_text()?;
        if typed.trim().is_empty() {
            ui.warn("no reset time; pacing assumes Monday 00:00 UTC until you run `powerqueue budget set-reset`");
            break;
        }
        match parse_reset_anchor(&typed) {
            Ok(at) => {
                answers.period_anchor = Some(at);
                ui.ok(&format!("period anchor {}", at.with_timezone(&Local).format("%a %Y-%m-%d %H:%M %Z")));
                break;
            }
            Err(e) => ui.fail(&format!("{e}")),
        }
    }
    Ok(())
}

fn keys_only(ctx: &mut Context, ui: &Ui, args: &InitArgs, interactive: bool) -> Result<i32> {
    let rt = super::runtime()?;
    let linear = linear_step(ctx, ui, args, interactive, &rt)?;
    let jev = jev_step(ctx, ui, args, interactive, &rt)?;
    if let Ok(mut cfg) = Config::load(&ctx.paths) {
        let changed = cfg.linear.enabled != linear.is_some() || (jev && !cfg.priority.jev.enabled);
        if changed {
            cfg.linear.enabled = linear.is_some();
            if jev {
                cfg.priority.jev.enabled = true;
            }
            cfg.save(&ctx.paths)?;
            ui.ok("config.toml updated");
        }
    }
    println!();
    println!(
        "{} keys updated. Run `powerqueue doctor` to verify everything.",
        "done".if_supports_color(Stream::Stdout, |t| t.style(Style::new().green().bold()))
    );
    Ok(0)
}

/// Handle `powerqueue init`.
pub fn run(ctx: &mut Context, args: InitArgs) -> Result<i32> {
    let interactive = !args.non_interactive && std::io::stdin().is_terminal();
    let ui = Ui::new(ctx.color);
    println!(
        "{}",
        if ctx.color {
            "powerqueue setup".if_supports_color(Stream::Stdout, |t| t.bold()).to_string()
        } else {
            "powerqueue setup".to_string()
        }
    );
    println!(
        "{}",
        format!("config directory: {}", ctx.paths.config_dir.display()).if_supports_color(Stream::Stdout, |t| t.dimmed())
    );

    if ctx.is_initialised() && !args.force {
        ui.warn(&format!("{} already exists", ctx.paths.config_file().display()));
        if !interactive {
            bail!("configuration already exists; use --force to overwrite it");
        }
        let choice = Select::with_theme(ui.theme())
            .with_prompt("What would you like to do?")
            .items(["Update the API keys only", "Start over (overwrite config.toml)", "Abort"])
            .default(0)
            .interact()?;
        match choice {
            0 => return keys_only(ctx, &ui, &args, interactive),
            1 => {}
            _ => {
                println!("aborted; nothing changed");
                return Ok(1);
            }
        }
    }

    if !interactive {
        let repo = detect_repo(args.repo.as_deref())?;
        let missing = missing_for_non_interactive(&args, Repo::new(&repo).is_repo());
        if !missing.is_empty() {
            bail!("non-interactive setup is missing:\n  - {}", missing.join("\n  - "));
        }
    }

    let mut answers = Answers::default();
    let (repo_path, default_branch) = repo_step(&ui, &args, interactive)?;
    answers.repo_path = repo_path;
    answers.default_branch = default_branch;

    let rt = super::runtime()?;
    let linear = linear_step(ctx, &ui, &args, interactive, &rt)?;
    answers.linear_enabled = linear.is_some();
    if let Some(client) = &linear {
        teams_step(&ui, client, &args, interactive, &mut answers, &rt)?;
    }

    let defaults = Config::default();
    tools_step(&ui, &defaults.claude.binary, &defaults.tmux.binary);
    answers.jev_enabled = jev_step(ctx, &ui, &args, interactive, &rt)?;
    tuning_step(&ui, interactive, &mut answers)?;

    ui.section("Writing files");
    let written = write_files(ctx, &answers)?;
    for w in &written {
        ui.ok(w);
    }

    println!();
    println!(
        "{}",
        if ctx.color {
            "all set".if_supports_color(Stream::Stdout, |t| t.style(Style::new().green().bold())).to_string()
        } else {
            "all set".to_string()
        }
    );
    println!("  repository     {}", answers.repo_path.display());
    println!(
        "  linear         {}",
        if answers.linear_enabled {
            format!("teams {}", if answers.team_keys.is_empty() { "all".to_string() } else { answers.team_keys.join(", ") })
        } else {
            "disabled".to_string()
        }
    );
    println!("  sessions       up to {} at once, permission mode {}", answers.max_concurrent, answers.permission_mode);
    println!("  weekly budget  {} weighted tokens", crate::cli::output::human_tokens(answers.period_weighted_tokens));
    println!();
    println!("next steps:");
    println!("  powerqueue doctor      verify the installation and the algorithm");
    println!("  powerqueue run         start the scheduler (keep it in tmux or a service)");
    println!("  powerqueue dashboard   watch sessions, budget and the queue");
    println!(
        "  {}",
        format!("edit {} to teach the scheduler what matters", ctx.paths.priority_file().display())
            .if_supports_color(Stream::Stdout, |t| t.dimmed())
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(name: &str, kind: &str) -> WorkflowState {
        WorkflowState { id: name.to_lowercase(), name: name.into(), kind: kind.into(), team_key: "ENG".into() }
    }

    #[test]
    fn default_states_prefer_todo_in_progress_in_review() {
        let states = vec![
            st("Backlog", "backlog"),
            st("Ready", "unstarted"),
            st("Todo", "unstarted"),
            st("Doing", "started"),
            st("In Progress", "started"),
            st("In Review", "started"),
            st("Done", "completed"),
        ];
        let (q, ip, d) = pick_default_states(&states);
        assert_eq!(q, vec!["Todo".to_string()]);
        assert_eq!(ip.as_deref(), Some("In Progress"));
        assert_eq!(d.as_deref(), Some("In Review"));
    }

    #[test]
    fn default_states_fall_back() {
        let states = vec![st("Backlog", "backlog"), st("Doing", "started"), st("Done", "completed"), st("Canceled", "canceled")];
        let (q, ip, d) = pick_default_states(&states);
        assert_eq!(q, vec!["Backlog".to_string()]);
        assert_eq!(ip.as_deref(), Some("Doing"));
        assert_eq!(d.as_deref(), Some("Done"));
        assert_eq!(pick_default_states(&[]), (vec![], None, None));
    }

    fn now_local() -> DateTime<Local> {
        // Wednesday 2026-03-04 15:30 local time.
        Local.from_local_datetime(&chrono::NaiveDate::from_ymd_opt(2026, 3, 4).unwrap().and_hms_opt(15, 30, 0).unwrap()).unwrap()
    }

    #[test]
    fn anchor_accepts_rfc3339_and_relative() {
        let at = parse_reset_anchor_at("2026-03-02T09:00:00Z", now_local()).unwrap();
        assert_eq!(at.to_rfc3339(), "2026-03-02T09:00:00+00:00");
        let rel = parse_reset_anchor_at("in 2h", now_local()).unwrap();
        assert_eq!(rel, now_local().with_timezone(&Utc) + chrono::Duration::hours(2));
        assert!(parse_reset_anchor_at("", now_local()).is_err());
        assert!(parse_reset_anchor_at("whenever", now_local()).is_err());
    }

    #[test]
    fn anchor_weekday_is_most_recent_occurrence() {
        let now = now_local();
        let mon = parse_reset_anchor_at("Mon 00:00", now).unwrap().with_timezone(&Local);
        assert_eq!(mon.weekday(), Weekday::Mon);
        assert_eq!(mon.date_naive(), chrono::NaiveDate::from_ymd_opt(2026, 3, 2).unwrap());
        assert_eq!(mon.time(), NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        // Same weekday, later time → previous week.
        let wed = parse_reset_anchor_at("wednesday 9:00pm", now).unwrap().with_timezone(&Local);
        assert_eq!(wed.date_naive(), chrono::NaiveDate::from_ymd_opt(2026, 2, 25).unwrap());
        assert_eq!(wed.time(), NaiveTime::from_hms_opt(21, 0, 0).unwrap());
        // Same weekday, earlier time → today.
        let today = parse_reset_anchor_at("Wed 9am", now).unwrap().with_timezone(&Local);
        assert_eq!(today.date_naive(), now.date_naive());
        let bare = parse_reset_anchor_at("09:00", now).unwrap().with_timezone(&Local);
        assert_eq!(bare.date_naive(), now.date_naive());
    }

    #[test]
    fn rendered_config_parses_and_keeps_header() {
        let answers = Answers {
            repo_path: PathBuf::from("/tmp/repo"),
            team_keys: vec!["ENG".into()],
            queued_states: vec!["Ready".into()],
            jev_enabled: true,
            max_concurrent: 3,
            permission_mode: "bypassPermissions".into(),
            period_anchor: Some(Utc.with_ymd_and_hms(2026, 3, 2, 9, 0, 0).unwrap()),
            ..Answers::default()
        };
        let text = render_config(&answers).unwrap();
        assert!(text.starts_with("# powerqueue configuration"));
        let cfg = Config::from_toml(&text).unwrap();
        assert_eq!(cfg.repo.path, "/tmp/repo");
        assert_eq!(cfg.linear.team_keys, vec!["ENG".to_string()]);
        assert_eq!(cfg.linear.queued_states, vec!["Ready".to_string()]);
        assert!(cfg.priority.jev.enabled);
        assert_eq!(cfg.scheduler.max_concurrent, 3);
        assert_eq!(cfg.claude.permission_mode, "bypassPermissions");
        assert_eq!(cfg.budget.period_anchor.as_deref(), Some("2026-03-02T09:00:00Z"));
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn non_interactive_reports_missing_items() {
        let args = InitArgs::default();
        let missing = missing_for_non_interactive(&args, false);
        assert_eq!(missing.len(), 2);
        let args = InitArgs { linear_key: Some("lin_x".into()), ..InitArgs::default() };
        assert!(missing_for_non_interactive(&args, true).is_empty());
        let args = InitArgs { no_linear: true, ..InitArgs::default() };
        assert!(missing_for_non_interactive(&args, true).is_empty());
    }

    #[test]
    fn write_files_creates_layout() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Context::new(Some(dir.path().to_path_buf()), crate::logging::Verbosity::Normal, false, false);
        let answers = Answers { repo_path: dir.path().to_path_buf(), ..Answers::default() };
        let written = write_files(&ctx, &answers).unwrap();
        assert_eq!(written.len(), 3);
        assert!(ctx.paths.config_file().exists());
        assert!(ctx.paths.priority_file().exists());
        assert!(ctx.paths.database().exists());
        Config::load(&ctx.paths).unwrap();
    }
}
