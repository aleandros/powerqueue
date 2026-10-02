//! Command-line interface.
//!
//! ```text
//! powerqueue init                 guided setup (keys, repo, team, providers, PRIORITY.md); --reconfigure edits an existing install
//! powerqueue run [--once]         start the scheduler daemon (foreground)
//! powerqueue stop                 ask a running daemon to exit
//! powerqueue dashboard            live TUI
//! powerqueue status               one-shot table
//! powerqueue add "title" [...]    enqueue a manual task
//! powerqueue task <show|list|complete|block|cancel|pause|resume|retry|explain|model>
//! powerqueue attach <task>        open the task's tmux window
//! powerqueue priority <show|check|edit|explain>
//! powerqueue budget <show|set-reset|set-observed|clear-limits|estimate> [--provider <p>]
//! powerqueue linear <teams|states|test|sync>
//! powerqueue doctor [--fix]       diagnostics + tuning advice
//! powerqueue logs [-f] [--task]   read the daemon log
//! powerqueue config <show|get|set|unset|path|edit|validate>
//! powerqueue secrets <set|unset|list>
//! powerqueue hook ...             (internal) called by agent CLI hooks (--provider claude|codex|gemini)
//! powerqueue completions <shell>
//! ```

pub mod commands;
pub mod context;
pub mod output;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::domain::{Criticality, ModelTier, Provider};

pub use context::Context;

/// Autonomous Linear → Claude Code work queue, one tmux window per task.
#[derive(Debug, Parser)]
#[command(name = "powerqueue", version, about, long_about = None, propagate_version = true)]
pub struct Cli {
    /// Increase log verbosity on stderr (-v debug, -vv trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
    /// Only print errors.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
    /// Override the base directory for config/data/state (default: XDG dirs).
    #[arg(long, global = true, env = "POWERQUEUE_HOME", value_name = "DIR")]
    pub home: Option<PathBuf>,
    /// Machine-readable JSON output where supported.
    #[arg(long, global = true)]
    pub json: bool,
    /// Disable colours (also honours the `NO_COLOR` environment variable).
    #[arg(long, global = true)]
    pub no_color: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Guided first-time setup: API keys, repository, Linear team, priority file.
    Init(InitArgs),
    /// Run the scheduler in the foreground (use tmux/launchd/systemd to keep it up).
    Run(RunArgs),
    /// Ask the running daemon to stop (sessions keep running in tmux).
    Stop,
    /// Live dashboard (needs an interactive terminal; `--once` works anywhere).
    #[command(alias = "ui", alias = "top")]
    Dashboard(DashboardArgs),
    /// One-shot status table.
    #[command(alias = "ls")]
    Status(StatusArgs),
    /// Add a manual task to the queue.
    Add(AddArgs),
    /// Inspect and control tasks.
    #[command(subcommand)]
    Task(TaskCommand),
    /// Attach to a task's tmux window (or the powerqueue session).
    Attach(AttachArgs),
    /// Priority rules (PRIORITY.md).
    #[command(subcommand)]
    Priority(PriorityCommand),
    /// Model budget and pacing.
    #[command(subcommand)]
    Budget(BudgetCommand),
    /// Linear helpers.
    #[command(subcommand)]
    Linear(LinearCommand),
    /// Diagnose the installation, the data and the scheduling algorithm.
    Doctor(DoctorArgs),
    /// Read the daemon log.
    Logs(LogsArgs),
    /// Configuration file helpers.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Manage stored API keys.
    #[command(subcommand)]
    Secrets(SecretsCommand),
    /// Internal: receive a Claude Code hook event.
    #[command(hide = true)]
    Hook(HookArgs),
    /// Generate shell completions.
    Completions { shell: clap_complete::Shell },
}

#[derive(Debug, Args, Default)]
pub struct DashboardArgs {
    /// Render one frame as plain text and exit (no TTY needed; with `--json` prints the snapshot).
    #[arg(long)]
    pub once: bool,
    /// Use ASCII symbols and borders (automatic when the locale is not UTF-8).
    #[arg(long)]
    pub ascii: bool,
}

#[derive(Debug, Args, Default)]
pub struct InitArgs {
    /// Repository path (default: current directory).
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
    /// Linear team key(s) to pull from, e.g. ENG.
    #[arg(long, value_name = "KEY")]
    pub team: Vec<String>,
    /// Linear API key (otherwise prompted; stored in the keychain).
    #[arg(long, env = "LINEAR_API_KEY", hide_env_values = true)]
    pub linear_key: Option<String>,
    /// Jev API key (optional).
    #[arg(long, env = "JEV_API_KEY", hide_env_values = true)]
    pub jev_key: Option<String>,
    /// Do not prompt; fail if something required is missing.
    #[arg(long)]
    pub non_interactive: bool,
    /// Skip Linear entirely (manual tasks only); can be enabled later in config.toml.
    #[arg(long)]
    pub no_linear: bool,
    /// Overwrite an existing configuration.
    #[arg(long)]
    pub force: bool,
    /// Permission mode for Claude Code sessions: acceptEdits | auto | bypassPermissions | dontAsk | plan | default.
    #[arg(long, value_name = "MODE")]
    pub permission_mode: Option<String>,
    /// Change the settings of an existing installation (keeps keys and everything not asked about).
    #[arg(long, conflicts_with = "force")]
    pub reconfigure: bool,
    /// Also run tasks on this provider (codex | gemini; repeatable). Claude is always on.
    /// Sets `budget.providers.<provider>.enabled` = true; interactive runs ask instead.
    #[arg(long, value_enum, value_name = "PROVIDER")]
    pub provider: Vec<Provider>,
}

#[derive(Debug, Args, Default)]
pub struct RunArgs {
    /// Perform one scheduling pass and exit.
    #[arg(long)]
    pub once: bool,
    /// Do not talk to Linear (manual tasks only).
    #[arg(long)]
    pub offline: bool,
}

#[derive(Debug, Args, Default)]
pub struct StatusArgs {
    /// Include completed/failed/cancelled tasks.
    #[arg(short, long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Task title.
    pub title: String,
    /// Longer description (markdown). `-` reads stdin.
    #[arg(short, long)]
    pub description: Option<String>,
    /// critical | high | normal | low.
    #[arg(short, long, value_parser = parse_criticality)]
    pub criticality: Option<Criticality>,
    /// Force a model: fable | opus | sonnet | haiku, or another provider's model (gpt-6.1-sol, gemini-3-pro, `codex:<name>`);
    /// warns when that provider is disabled in config.
    #[arg(short, long, value_parser = parse_model)]
    pub model: Option<ModelTier>,
    /// Explicit key (default: `manual-<id>`).
    #[arg(short, long)]
    pub key: Option<String>,
    /// Labels for PRIORITY.md rules.
    #[arg(short, long)]
    pub label: Vec<String>,
    /// Start paused.
    #[arg(long)]
    pub paused: bool,
}

#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Show a task's details and timeline.
    Show(TaskRef),
    /// List tasks (same as `status`).
    List(StatusArgs),
    /// Mark a task completed (also used by Claude from inside the session).
    Complete {
        #[command(flatten)]
        task: TaskRef,
        #[arg(short, long)]
        summary: Option<String>,
    },
    /// Mark a task blocked / needing a human.
    Block {
        #[command(flatten)]
        task: TaskRef,
        #[arg(short, long)]
        reason: Option<String>,
    },
    /// Cancel a task and release its resources.
    Cancel(TaskRef),
    /// Pause (do not schedule / stop after current turn).
    Pause(TaskRef),
    /// Resume a paused or needs-attention task.
    Resume(TaskRef),
    /// Re-queue a failed, cancelled or completed task.
    Retry(TaskRef),
    /// Explain the current score and model decision.
    Explain(TaskRef),
    /// Force (or clear with `auto`) the model for the next attempt; any provider's model is accepted.
    Model {
        #[command(flatten)]
        task: TaskRef,
        /// fable | opus | sonnet | haiku | auto, or another provider's model (gpt-6.1-sol, gemini-3-pro, `codex:<name>`);
        /// warns when that provider is disabled in config
        model: String,
    },
    /// Print the last screen of the task's tmux pane.
    Output {
        #[command(flatten)]
        task: TaskRef,
        #[arg(short = 'n', long, default_value_t = 60)]
        lines: u32,
    },
    /// Send a message to the running session (as if typed).
    Send {
        #[command(flatten)]
        task: TaskRef,
        message: String,
    },
}

#[derive(Debug, Args, Clone)]
pub struct TaskRef {
    /// Task key (ENG-123), id, or id prefix.
    pub task: String,
}

#[derive(Debug, Args)]
pub struct AttachArgs {
    /// Task key or id. Omit to attach to the powerqueue session itself.
    pub task: Option<String>,
    /// Print the tmux command instead of executing it.
    #[arg(long)]
    pub print: bool,
}

#[derive(Debug, Subcommand)]
pub enum PriorityCommand {
    /// Print the parsed rules.
    Show,
    /// Validate the file and report problems.
    Check,
    /// Open PRIORITY.md in $EDITOR.
    Edit,
    /// Show how the rules score one task.
    Explain(TaskRef),
    /// Print the path of the rules file.
    Path,
}

#[derive(Debug, Subcommand)]
pub enum BudgetCommand {
    /// Show period/window spend per provider and model, observed usage, and what the policy would allow.
    Show,
    /// Record a provider's period reset instant (from `/usage` in Claude Code; RFC 3339 or "in 3d4h").
    SetReset {
        when: String,
        /// Which provider's budget to set.
        #[arg(long, value_enum, default_value_t = Provider::Claude)]
        provider: Provider,
    },
    /// Calibrate a provider's pacing with the usage percentage it shows (e.g. `43%`).
    SetObserved {
        percent: String,
        /// Which provider's budget to calibrate.
        #[arg(long, value_enum, default_value_t = Provider::Claude)]
        provider: Provider,
    },
    /// Ask providers for their remaining allowance now (Codex app-server, Claude status line, agy /usage).
    Probe {
        /// Probe only this provider (default: every enabled provider).
        #[arg(long, value_enum)]
        provider: Option<Provider>,
    },
    /// Forget a provider's rate-limit cooldowns.
    ClearLimits {
        /// Which provider's cooldowns to forget.
        #[arg(long, value_enum, default_value_t = Provider::Claude)]
        provider: Provider,
    },
    /// Show the cost estimator's view of a task (or all history).
    Estimate { task: Option<String> },
}

#[derive(Debug, Subcommand)]
pub enum LinearCommand {
    /// List teams visible to the API key.
    Teams,
    /// List workflow states of a team.
    States { team: String },
    /// Verify the API key (prints the viewer).
    Test,
    /// Fetch queued issues now and print what would change.
    Sync {
        /// Apply changes to the local queue.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Args, Default)]
pub struct DoctorArgs {
    /// Apply safe automatic fixes.
    #[arg(long)]
    pub fix: bool,
    /// Skip network checks.
    #[arg(long)]
    pub offline: bool,
}

#[derive(Debug, Args, Default)]
pub struct LogsArgs {
    /// Follow (like `tail -f`).
    #[arg(short, long)]
    pub follow: bool,
    /// Number of lines to show.
    #[arg(short = 'n', long, default_value_t = 200)]
    pub lines: usize,
    /// Only lines mentioning this task (key or id).
    #[arg(short, long)]
    pub task: Option<String>,
    /// Minimum level: trace|debug|info|warn|error.
    #[arg(short, long)]
    pub level: Option<String>,
    /// Show the task event timeline from the database instead of the log file.
    #[arg(long)]
    pub events: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the effective configuration (TOML).
    Show,
    /// Print one value by dotted key, e.g. `claude.permission_mode` (TOML; `--json` for JSON).
    Get { key: String },
    /// Set one value by dotted key; the value is TOML (`3`, `true`, `["ENG","OPS"]`) or a bare string.
    Set { key: String, value: String },
    /// Remove one key from config.toml so its default applies again.
    Unset { key: String },
    /// Print config/data/state paths.
    Path,
    /// Open config.toml in $EDITOR.
    Edit,
    /// Validate config.toml and the repo's .powerqueue.toml.
    Validate,
}

#[derive(Debug, Subcommand)]
pub enum SecretsCommand {
    /// Store a key (prompts if value omitted). NAME: linear | jev
    Set { name: String, value: Option<String> },
    /// Remove a key.
    Unset { name: String },
    /// Show which keys are configured and where they come from.
    List,
}

#[derive(Debug, Args)]
pub struct HookArgs {
    #[arg(long)]
    pub task: String,
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long)]
    pub event: String,
    /// Which agent CLI sent the hook; payloads of other providers are normalised to the Claude shape.
    #[arg(long, value_enum, default_value_t = Provider::Claude)]
    pub provider: Provider,
    /// The JSON payload as an argument (Codex `notify` appends it); read from stdin when absent.
    #[arg(value_name = "PAYLOAD")]
    pub payload: Option<String>,
}

fn parse_criticality(s: &str) -> Result<Criticality, String> {
    s.parse()
}
fn parse_model(s: &str) -> Result<ModelTier, String> {
    s.parse()
}

impl ValueEnum for Criticality {
    fn value_variants<'a>() -> &'a [Self] {
        &Criticality::ALL
    }
    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.as_str()))
    }
}

impl ValueEnum for Provider {
    fn value_variants<'a>() -> &'a [Self] {
        &Provider::ALL
    }
    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        let v = clap::builder::PossibleValue::new(self.as_str());
        Some(match self {
            Provider::Gemini => v.aliases(["antigravity", "agy"]),
            _ => v,
        })
    }
}

/// Dispatch a parsed command. Returns the process exit code.
pub fn run(cli: Cli) -> anyhow::Result<i32> {
    commands::dispatch(cli)
}

/// Print an error chain to stderr in a friendly format.
pub fn report_error(err: &anyhow::Error) {
    output::print_error(err);
}
