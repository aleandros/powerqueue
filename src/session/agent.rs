//! The provider abstraction: what the launcher, the transcript tailer, the
//! hook receiver and `doctor` need from a coding-agent CLI.
//!
//! The implementations live next to this file: [`crate::session::claude`]
//! (Claude Code), [`crate::session::codex`] (OpenAI Codex CLI) and
//! [`crate::session::gemini`] (Google Antigravity CLI, experimental). This
//! module keeps the trait, [`agent_for`] and the helpers they share (prompt
//! argument, environment merging, rate-limit signature matching).
//!
//! Everything provider-neutral (`launch.sh`, `prompt.md`, `env`, the tmux
//! window, resource sampling, crash handling, cleanup) stays in
//! [`crate::session::launcher`] and the scheduler.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::budget::ObservedUsage;
use crate::config::Config;
use crate::domain::{HookEvent, ModelTier, Provider, Task, TaskId, UsageRecord};
use crate::paths::Paths;
use crate::session::transcript::TranscriptState;
use crate::store::Store;
use crate::tmux::shell_quote;

pub use crate::session::claude::ClaudeCli;
pub use crate::session::codex::CodexCli;
pub use crate::session::gemini::GeminiCli;

/// Everything a provider needs to build its launch.
#[derive(Debug, Clone)]
pub struct LaunchContext<'a> {
    pub cfg: &'a Config,
    /// powerqueue's directory layout (data and state dirs must stay writable
    /// from inside sandboxed sessions so `powerqueue task complete` works).
    pub paths: &'a Paths,
    pub task: &'a Task,
    /// powerqueue's session id (Claude Code's `--session-id`).
    pub session_id: uuid::Uuid,
    pub model: &'a ModelTier,
    pub attempt: u32,
    /// The provider's session id to resume (`Some` after a crash), `None`
    /// for a fresh start. For Claude this is `session_id` itself.
    pub resume: Option<&'a str>,
    /// `<state>/tasks/<task id>/`, already created.
    pub task_dir: &'a Path,
    /// `prompt.md`, already written.
    pub prompt_path: &'a Path,
    pub worktree: &'a Path,
    /// Absolute path of the running `powerqueue` binary (for hooks).
    pub self_bin: &'a Path,
}

/// What a provider wants on disk and on the command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentLaunch {
    /// Files to write before launching: `(path, contents, unix mode)`.
    /// Parent directories are created.
    pub files: Vec<(PathBuf, String, u32)>,
    /// Environment exported by `launch.sh` (provider env plus `POWERQUEUE_*`).
    pub env: Vec<(String, String)>,
    /// The command line; the last element is the prompt argument
    /// (see [`prompt_arg`]).
    pub argv: Vec<String>,
    /// Where the CLI will write its transcript, when known at launch time.
    pub transcript_path: Option<PathBuf>,
    /// Scan the transcript for the DONE / BLOCKED markers as a fallback
    /// completion signal (CLIs without a reliable `Stop` hook).
    pub poll_transcript_for_completion: bool,
}

/// Outcome of a provider's login check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStatus {
    pub logged_in: bool,
    /// Human summary (account, method, or the CLI's own words).
    pub detail: String,
}

/// A coding-agent CLI.
pub trait AgentCli: Send + Sync {
    fn provider(&self) -> Provider;
    /// Build files, env and argv for a launch. Fails when the launch cannot
    /// be described (e.g. a resume without a provider session id).
    fn prepare(&self, ctx: &LaunchContext<'_>) -> Result<AgentLaunch>;
    /// Run once right before the tmux window is created (trust seeding,
    /// git excludes for hook files in the worktree). Must not fail for
    /// cosmetic problems.
    fn pre_launch(&self, cfg: &Config, repo: &Path, worktree: &Path) -> Result<()>;
    /// True when the CLI accepts powerqueue's session id (`--session-id`),
    /// false when it generates its own and [`AgentCli::discover_session`]
    /// must find it. Sessions of the latter kind are only resumed when the
    /// id was discovered.
    fn accepts_session_id(&self) -> bool {
        false
    }
    /// For CLIs that generate their own session id: find the session started
    /// after `started_after` in `worktree` and return `(agent session id,
    /// transcript path)`. `Ok(None)` while nothing is there yet.
    fn discover_session(&self, cfg: &Config, worktree: &Path, started_after: DateTime<Utc>) -> Result<Option<(String, PathBuf)>>;
    /// Parse one transcript line into a usage record, if it carries usage.
    /// `launched_model` is the model in effect: the launched model, or the
    /// one the transcript itself last reported (see
    /// [`AgentCli::observe_transcript_line`]).
    fn parse_transcript_line(
        &self,
        line: &str,
        session_id: uuid::Uuid,
        task_id: TaskId,
        launched_model: &ModelTier,
    ) -> Option<UsageRecord>;
    /// Update per-reader state from one transcript line (current model,
    /// rate-limit snapshots and errors, the last assistant message). Called
    /// before [`AgentCli::parse_transcript_line`] for every complete line.
    fn observe_transcript_line(&self, _line: &str, _state: &mut TranscriptState) {}
    /// True when the daemon should scan the transcript for the DONE /
    /// BLOCKED markers (mirrors [`AgentLaunch::poll_transcript_for_completion`]).
    fn polls_transcript_for_completion(&self) -> bool {
        false
    }
    /// Map a provider hook (`--event <name>` plus its payload) to the
    /// Claude-shaped event the rest of the daemon understands. `None` = ignore.
    fn normalize_hook(&self, event: &str, payload: serde_json::Value) -> Option<(HookEvent, serde_json::Value)>;
    /// Lower-case substrings that mark an error message or pane text as "the
    /// account is throttled" rather than "this turn broke".
    fn rate_limit_signatures(&self) -> &'static [&'static str];
    /// Check whether `binary` is logged in. Errors only when the binary
    /// cannot be executed at all.
    fn auth_status(&self, binary: &str) -> Result<AuthStatus>;
    /// Values the provider's mode setting accepts (`claude.permission_mode`,
    /// `codex.approval`, `gemini.mode`).
    fn allowed_modes(&self) -> &'static [&'static str];
    /// Ask the provider how much allowance is left. `Ok(None)` = unknown.
    fn probe(&self, cfg: &Config, store: &Store) -> Result<Option<ObservedUsage>>;
    /// Whether a session launched with the current settings can run `git
    /// commit` itself. When false, the prompt tells the agent to leave its
    /// changes in the working tree and cleanup commits them
    /// (`cleanup.commit_uncommitted`).
    fn commits_in_session(&self, _cfg: &Config) -> bool {
        true
    }
}

static CLAUDE: ClaudeCli = ClaudeCli;
static CODEX: CodexCli = CodexCli;
static GEMINI: GeminiCli = GeminiCli;

/// The implementation for a provider.
pub fn agent_for(provider: Provider) -> &'static dyn AgentCli {
    match provider {
        Provider::Claude => &CLAUDE,
        Provider::Codex => &CODEX,
        Provider::Gemini => &GEMINI,
    }
}

/// `$(cat '<path>')`: how the prompt is passed on the command line.
pub fn prompt_arg(prompt_path: &Path) -> String {
    format!("$(cat {})", shell_quote(&prompt_path.to_string_lossy()))
}

/// Environment exported to a session: the provider's `env` table plus
/// powerqueue's own variables (so `powerqueue task complete` inside the
/// session finds the same home as the daemon). `POWERQUEUE_BIN` names the
/// `powerqueue` command the session should run (`powerqueue_bin`: the host
/// binary, or the task's shim with `<provider>.shim`) unless the provider's
/// `env` sets it.
pub fn session_env(
    provider_env: &BTreeMap<String, String>,
    task: &Task,
    session_id: uuid::Uuid,
    powerqueue_bin: &Path,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = provider_env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    env.push(("POWERQUEUE_TASK_ID".to_string(), task.id.to_string()));
    env.push(("POWERQUEUE_TASK_KEY".to_string(), task.key.clone()));
    env.push(("POWERQUEUE_SESSION_ID".to_string(), session_id.to_string()));
    if !provider_env.contains_key("POWERQUEUE_BIN") {
        env.push(("POWERQUEUE_BIN".to_string(), powerqueue_bin.to_string_lossy().to_string()));
    }
    if let Some(home) = std::env::var_os("POWERQUEUE_HOME") {
        env.push(("POWERQUEUE_HOME".to_string(), home.to_string_lossy().to_string()));
    }
    env
}

/// True when `text` contains one of `signatures` (lower-case), ignoring case
/// and treating the typographic apostrophe `’` like `'`.
pub fn matches_signature(text: &str, signatures: &[&str]) -> bool {
    let lower = text.to_lowercase().replace('\u{2019}', "'");
    signatures.iter().any(|s| lower.contains(&s.to_lowercase().replace('\u{2019}', "'")))
}

/// First non-empty line of stdout, else stderr, else `fallback`.
pub(crate) fn first_line(stdout: &[u8], stderr: &[u8], fallback: &str) -> String {
    for bytes in [stdout, stderr] {
        let text = String::from_utf8_lossy(bytes);
        if let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty()) {
            return line.to_string();
        }
    }
    fallback.to_string()
}

/// The canonical form of `path` when it resolves, else `path` itself.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// True when `a` and `b` name the same directory (compared canonicalised).
pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || canonical(a) == canonical(b)
}

/// `$HOME` (or `.` when unset).
pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CLAUDE_PERMISSION_MODES, CODEX_APPROVAL_MODES, GEMINI_MODES};

    #[test]
    fn agent_for_matches_provider() {
        for p in Provider::ALL {
            assert_eq!(agent_for(p).provider(), p);
            assert!(!agent_for(p).allowed_modes().is_empty());
            assert!(!agent_for(p).rate_limit_signatures().is_empty());
        }
        assert_eq!(agent_for(Provider::Claude).allowed_modes(), &CLAUDE_PERMISSION_MODES);
        assert_eq!(agent_for(Provider::Codex).allowed_modes(), &CODEX_APPROVAL_MODES);
        assert_eq!(agent_for(Provider::Gemini).allowed_modes(), &GEMINI_MODES);
        assert!(agent_for(Provider::Claude).accepts_session_id());
        assert!(!agent_for(Provider::Codex).accepts_session_id());
        assert!(!agent_for(Provider::Gemini).accepts_session_id());
        assert!(agent_for(Provider::Gemini).polls_transcript_for_completion());
        assert!(!agent_for(Provider::Codex).polls_transcript_for_completion());
    }

    #[test]
    fn signatures_ignore_case_and_apostrophe_style() {
        let sigs = ["hit your usage limit", "quota exceeded"];
        assert!(matches_signature("You’ve hit your usage limit. Try again at 3pm.", &sigs));
        assert!(matches_signature("YOU'VE HIT YOUR USAGE LIMIT", &sigs));
        assert!(matches_signature("Quota exceeded. Check your plan", &sigs));
        assert!(!matches_signature("all good", &sigs));
        assert!(matches_signature("you’ve", &["you've"]));
    }

    #[test]
    fn session_env_merges_provider_env() {
        let t = Task::new("ENG-1", "t", crate::domain::TaskSource::Manual);
        let mut base = BTreeMap::new();
        base.insert("A".to_string(), "1".to_string());
        let sid = uuid::Uuid::new_v4();
        let env = session_env(&base, &t, sid, Path::new("/opt/pq/bin/powerqueue"));
        assert_eq!(env[0], ("A".to_string(), "1".to_string()));
        assert!(env.contains(&("POWERQUEUE_SESSION_ID".to_string(), sid.to_string())));
        assert!(env.contains(&("POWERQUEUE_TASK_KEY".to_string(), "ENG-1".to_string())));
        assert!(env.contains(&("POWERQUEUE_BIN".to_string(), "/opt/pq/bin/powerqueue".to_string())));
        // The provider env wins over the default.
        base.insert("POWERQUEUE_BIN".to_string(), "fake".to_string());
        let env = session_env(&base, &t, sid, Path::new("/opt/pq/bin/powerqueue"));
        assert_eq!(env.iter().filter(|(k, _)| k == "POWERQUEUE_BIN").count(), 1);
        assert!(env.contains(&("POWERQUEUE_BIN".to_string(), "fake".to_string())));
    }

    #[test]
    fn helpers() {
        assert_eq!(first_line(b"", b"", "fallback"), "fallback");
        assert_eq!(first_line(b"\n a \n", b"b", "f"), "a");
        assert_eq!(first_line(b"", b"err\n", "f"), "err");
        assert_eq!(prompt_arg(Path::new("/s/prompt.md")), "$(cat /s/prompt.md)");
        let dir = tempfile::tempdir().unwrap();
        assert!(same_dir(dir.path(), &canonical(dir.path())));
        assert!(!same_dir(dir.path(), Path::new("/definitely/elsewhere")));
    }
}
