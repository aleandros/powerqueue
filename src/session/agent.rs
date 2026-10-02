//! The provider abstraction: what the launcher, the transcript tailer, the
//! hook receiver and `doctor` need from a coding-agent CLI.
//!
//! [`ClaudeCli`] wraps today's Claude Code integration (hook settings,
//! `claude_command`, trust seeding, transcript path and parsing). [`CodexCli`]
//! and [`GeminiCli`] know their provider, their modes, their rate-limit
//! signatures and how to check authentication; their launchers land on the
//! session branch and the remaining methods say so with an error.
//!
//! Everything provider-neutral (`launch.sh`, `prompt.md`, `env`, the tmux
//! window, resource sampling, crash handling, cleanup) stays in
//! [`crate::session::launcher`] and the scheduler.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use crate::budget::ObservedUsage;
use crate::config::{CLAUDE_PERMISSION_MODES, CODEX_APPROVAL_MODES, Config, GEMINI_MODES};
use crate::domain::{HookEvent, ModelTier, Provider, Task, TaskId, UsageRecord};
use crate::store::Store;

/// Everything a provider needs to build its launch.
#[derive(Debug, Clone)]
pub struct LaunchContext<'a> {
    pub cfg: &'a Config,
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
    pub files: Vec<(PathBuf, String, u32)>,
    /// Environment exported by `launch.sh` (provider env plus `POWERQUEUE_*`).
    pub env: Vec<(String, String)>,
    /// The command line; the last element is the prompt argument
    /// (see [`crate::session::launcher::prompt_arg`]).
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
    /// Build files, env and argv for a launch. Fails when the provider's
    /// launcher is not available in this version.
    fn prepare(&self, ctx: &LaunchContext<'_>) -> Result<AgentLaunch>;
    /// Run once right before the tmux window is created (trust seeding,
    /// hook files in the worktree). Must not fail for cosmetic problems.
    fn pre_launch(&self, cfg: &Config, repo: &Path, worktree: &Path) -> Result<()>;
    /// For CLIs that generate their own session id: find the session started
    /// after `started_after` in `worktree` and return `(agent session id,
    /// transcript path)`. `Ok(None)` while nothing is there yet.
    fn discover_session(&self, cfg: &Config, worktree: &Path, started_after: DateTime<Utc>) -> Result<Option<(String, PathBuf)>>;
    /// Parse one transcript line into a usage record, if it carries usage.
    fn parse_transcript_line(
        &self,
        line: &str,
        session_id: uuid::Uuid,
        task_id: TaskId,
        launched_model: &ModelTier,
    ) -> Option<UsageRecord>;
    /// Map a provider hook (`--event <name>` plus its stdin payload) to the
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

// ------------------------------------------------------------------ claude

/// Claude Code: `--session-id`, hook settings, JSONL transcripts.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeCli;

impl AgentCli for ClaudeCli {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn prepare(&self, ctx: &LaunchContext<'_>) -> Result<AgentLaunch> {
        use crate::session::launcher::{Launcher, hook_settings, session_env};
        let settings_path = ctx.task_dir.join("settings.json");
        let settings = hook_settings(ctx.self_bin, ctx.task.id, ctx.session_id, &ctx.cfg.claude);
        let settings_text = serde_json::to_string_pretty(&settings).context("serialise settings.json")? + "\n";
        let argv = Launcher::claude_command(
            ctx.cfg,
            ctx.model,
            ctx.session_id,
            &settings_path,
            ctx.prompt_path,
            ctx.resume.is_some(),
            &ctx.task.key,
        );
        // Claude Code encodes its *physical* cwd, so resolve symlinks (e.g. /tmp → /private/tmp).
        let physical = std::fs::canonicalize(ctx.worktree).unwrap_or_else(|_| ctx.worktree.to_path_buf());
        let transcript = crate::session::transcript::transcript_path_for(
            &crate::session::transcript::claude_home(),
            &physical,
            ctx.session_id,
        );
        Ok(AgentLaunch {
            files: vec![(settings_path, settings_text, 0o644)],
            env: session_env(ctx.cfg, ctx.task, ctx.session_id),
            argv,
            transcript_path: Some(transcript),
            poll_transcript_for_completion: false,
        })
    }

    /// Pre-seed workspace trust in `~/.claude.json` when
    /// `claude.trust_workspace` is set. A failure only warns: the session
    /// may then wait on the trust dialog, which is what it did before.
    fn pre_launch(&self, cfg: &Config, repo: &Path, worktree: &Path) -> Result<()> {
        if !cfg.claude.trust_workspace {
            return Ok(());
        }
        let file = crate::session::trust::claude_json_path();
        let targets = crate::session::trust::trust_targets(repo, worktree);
        let refs: Vec<&Path> = targets.iter().map(|p| p.as_path()).collect();
        match crate::session::trust::ensure_trusted(&file, &refs) {
            Ok(newly) if !newly.is_empty() => {
                tracing::info!(file = %file.display(), paths = ?newly, "marked workspace as trusted for Claude Code")
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "could not pre-trust workspace; the session may wait on the trust dialog")
            }
        }
        Ok(())
    }

    fn discover_session(
        &self,
        _cfg: &Config,
        _worktree: &Path,
        _started_after: DateTime<Utc>,
    ) -> Result<Option<(String, PathBuf)>> {
        // powerqueue picks the id (`--session-id`); nothing to discover.
        Ok(None)
    }

    fn parse_transcript_line(
        &self,
        line: &str,
        session_id: uuid::Uuid,
        task_id: TaskId,
        _launched_model: &ModelTier,
    ) -> Option<UsageRecord> {
        crate::session::transcript::parse_line(line, session_id, task_id)
    }

    fn normalize_hook(&self, event: &str, payload: serde_json::Value) -> Option<(HookEvent, serde_json::Value)> {
        event.parse::<HookEvent>().ok().map(|e| (e, payload))
    }

    fn rate_limit_signatures(&self) -> &'static [&'static str] {
        &["usage limit reached", "rate_limit", "rate limit", "overloaded"]
    }

    fn auth_status(&self, binary: &str) -> Result<AuthStatus> {
        let a = crate::doctor::claude_auth_status(binary)?;
        Ok(AuthStatus { logged_in: a.logged_in, detail: a.detail })
    }

    fn allowed_modes(&self) -> &'static [&'static str] {
        &CLAUDE_PERMISSION_MODES
    }

    fn probe(&self, _cfg: &Config, _store: &Store) -> Result<Option<ObservedUsage>> {
        Ok(None)
    }
}

// ------------------------------------------------------------------- codex

/// OpenAI Codex CLI. Launching lands on the session branch.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexCli;

const NOT_YET: &str = "launcher is implemented on the session branch";

impl AgentCli for CodexCli {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn prepare(&self, _ctx: &LaunchContext<'_>) -> Result<AgentLaunch> {
        bail!("codex {NOT_YET}")
    }

    fn pre_launch(&self, _cfg: &Config, _repo: &Path, _worktree: &Path) -> Result<()> {
        bail!("codex {NOT_YET}")
    }

    fn discover_session(
        &self,
        _cfg: &Config,
        _worktree: &Path,
        _started_after: DateTime<Utc>,
    ) -> Result<Option<(String, PathBuf)>> {
        bail!("codex {NOT_YET}")
    }

    fn parse_transcript_line(
        &self,
        _line: &str,
        _session_id: uuid::Uuid,
        _task_id: TaskId,
        _launched_model: &ModelTier,
    ) -> Option<UsageRecord> {
        None
    }

    fn normalize_hook(&self, _event: &str, _payload: serde_json::Value) -> Option<(HookEvent, serde_json::Value)> {
        None
    }

    fn rate_limit_signatures(&self) -> &'static [&'static str] {
        &["hit your usage limit", "usage limit reached", "rate limit exceeded", "quota exceeded", "out of credits"]
    }

    /// `codex login status`: exit 0 means logged in; the first output line is the detail.
    fn auth_status(&self, binary: &str) -> Result<AuthStatus> {
        let out = std::process::Command::new(binary)
            .args(["login", "status"])
            .output()
            .with_context(|| format!("run `{binary} login status`"))?;
        Ok(AuthStatus { logged_in: out.status.success(), detail: first_line(&out.stdout, &out.stderr, "not logged in") })
    }

    fn allowed_modes(&self) -> &'static [&'static str] {
        &CODEX_APPROVAL_MODES
    }

    fn probe(&self, _cfg: &Config, _store: &Store) -> Result<Option<ObservedUsage>> {
        Ok(None)
    }
}

// ------------------------------------------------------------------ gemini

/// Google Antigravity CLI (`agy`). Experimental; launching lands on the session branch.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiCli;

impl AgentCli for GeminiCli {
    fn provider(&self) -> Provider {
        Provider::Gemini
    }

    fn prepare(&self, _ctx: &LaunchContext<'_>) -> Result<AgentLaunch> {
        bail!("gemini (agy) {NOT_YET}")
    }

    fn pre_launch(&self, _cfg: &Config, _repo: &Path, _worktree: &Path) -> Result<()> {
        bail!("gemini (agy) {NOT_YET}")
    }

    fn discover_session(
        &self,
        _cfg: &Config,
        _worktree: &Path,
        _started_after: DateTime<Utc>,
    ) -> Result<Option<(String, PathBuf)>> {
        bail!("gemini (agy) {NOT_YET}")
    }

    fn parse_transcript_line(
        &self,
        _line: &str,
        _session_id: uuid::Uuid,
        _task_id: TaskId,
        _launched_model: &ModelTier,
    ) -> Option<UsageRecord> {
        None
    }

    fn normalize_hook(&self, _event: &str, _payload: serde_json::Value) -> Option<(HookEvent, serde_json::Value)> {
        None
    }

    fn rate_limit_signatures(&self) -> &'static [&'static str] {
        &["resource_exhausted", "quota reached", "individual quota reached", "code 429"]
    }

    /// `agy --version`: the CLI has no login-status command, so a runnable
    /// binary counts as "logged in" and the version is the detail.
    fn auth_status(&self, binary: &str) -> Result<AuthStatus> {
        let out =
            std::process::Command::new(binary).arg("--version").output().with_context(|| format!("run `{binary} --version`"))?;
        Ok(AuthStatus {
            logged_in: out.status.success(),
            detail: format!("{} (login state not verifiable)", first_line(&out.stdout, &out.stderr, "no version output")),
        })
    }

    fn allowed_modes(&self) -> &'static [&'static str] {
        &GEMINI_MODES
    }

    fn probe(&self, _cfg: &Config, _store: &Store) -> Result<Option<ObservedUsage>> {
        Ok(None)
    }
}

/// First non-empty line of stdout, else stderr, else `fallback`.
fn first_line(stdout: &[u8], stderr: &[u8], fallback: &str) -> String {
    for bytes in [stdout, stderr] {
        let text = String::from_utf8_lossy(bytes);
        if let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty()) {
            return line.to_string();
        }
    }
    fallback.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskSource;

    fn task(dir: &Path) -> Task {
        let mut t = Task::new("ENG-7", "Do the thing", TaskSource::Manual);
        t.worktree_path = Some(dir.join("wt").display().to_string());
        t
    }

    #[test]
    fn agent_for_matches_provider() {
        for p in Provider::ALL {
            assert_eq!(agent_for(p).provider(), p);
            assert!(!agent_for(p).allowed_modes().is_empty());
            assert!(!agent_for(p).rate_limit_signatures().is_empty());
        }
        assert_eq!(agent_for(Provider::Claude).allowed_modes(), &CLAUDE_PERMISSION_MODES);
        assert_eq!(agent_for(Provider::Codex).allowed_modes(), &CODEX_APPROVAL_MODES);
    }

    #[test]
    fn claude_prepare_matches_the_launcher_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.repo.path = "/work/repo".into();
        cfg.claude.env.insert("FOO".into(), "bar".into());
        let t = task(dir.path());
        let sid = uuid::Uuid::new_v4();
        let model = ModelTier::opus();
        let ctx = LaunchContext {
            cfg: &cfg,
            task: &t,
            session_id: sid,
            model: &model,
            attempt: 2,
            resume: None,
            task_dir: dir.path(),
            prompt_path: &dir.path().join("prompt.md"),
            worktree: Path::new(t.worktree_path.as_deref().unwrap()),
            self_bin: Path::new("/bin/powerqueue"),
        };
        let launch = agent_for(Provider::Claude).prepare(&ctx).unwrap();
        assert_eq!(launch.files.len(), 1);
        assert_eq!(launch.files[0].0, dir.path().join("settings.json"));
        assert_eq!(launch.files[0].2, 0o644);
        let settings: serde_json::Value = serde_json::from_str(&launch.files[0].1).unwrap();
        assert_eq!(settings["hooks"].as_object().unwrap().len(), 7);
        assert_eq!(launch.argv[0], "claude");
        assert_eq!(launch.argv[1], "--session-id");
        assert!(launch.argv.contains(&"opus".to_string()));
        assert!(launch.env.iter().any(|(k, v)| k == "FOO" && v == "bar"));
        assert!(launch.env.iter().any(|(k, _)| k == "POWERQUEUE_SESSION_ID"));
        assert!(launch.transcript_path.unwrap().to_string_lossy().ends_with(&format!("{sid}.jsonl")));
        assert!(!launch.poll_transcript_for_completion);

        let sid_s = sid.to_string();
        let ctx = LaunchContext { resume: Some(&sid_s), ..ctx };
        let launch = agent_for(Provider::Claude).prepare(&ctx).unwrap();
        assert_eq!(launch.argv[1], "--resume");
    }

    #[test]
    fn claude_hook_and_transcript_delegation() {
        let claude = agent_for(Provider::Claude);
        let (event, payload) = claude.normalize_hook("Stop", serde_json::json!({ "a": 1 })).unwrap();
        assert_eq!(event, HookEvent::Stop);
        assert_eq!(payload["a"], 1);
        assert!(claude.normalize_hook("Bogus", serde_json::json!({})).is_none());
        let line = r#"{"type":"assistant","message":{"id":"msg_1","model":"claude-opus-5-5","usage":{"output_tokens":3}}}"#;
        let rec = claude.parse_transcript_line(line, uuid::Uuid::new_v4(), TaskId::new(), &ModelTier::sonnet()).unwrap();
        assert_eq!(rec.tier, ModelTier::opus());
        assert!(claude.parse_transcript_line("not json", uuid::Uuid::new_v4(), TaskId::new(), &ModelTier::sonnet()).is_none());
        assert!(claude.discover_session(&Config::default(), Path::new("/x"), Utc::now()).unwrap().is_none());
        let store = Store::open_in_memory().unwrap();
        assert!(claude.probe(&Config::default(), &store).unwrap().is_none());
    }

    #[test]
    fn claude_pre_launch_respects_trust_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.claude.trust_workspace = false;
        agent_for(Provider::Claude).pre_launch(&cfg, dir.path(), &dir.path().join("wt")).unwrap();
    }

    #[test]
    fn stubs_say_where_the_launcher_lives() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        let t = task(dir.path());
        let model = ModelTier::new("gpt-6.1-sol");
        let ctx = LaunchContext {
            cfg: &cfg,
            task: &t,
            session_id: uuid::Uuid::new_v4(),
            model: &model,
            attempt: 1,
            resume: None,
            task_dir: dir.path(),
            prompt_path: &dir.path().join("prompt.md"),
            worktree: dir.path(),
            self_bin: Path::new("/bin/powerqueue"),
        };
        for p in [Provider::Codex, Provider::Gemini] {
            let agent = agent_for(p);
            let err = agent.prepare(&ctx).unwrap_err().to_string();
            assert!(err.contains("launcher is implemented on the session branch"), "{err}");
            assert!(agent.pre_launch(&cfg, dir.path(), dir.path()).is_err());
            assert!(agent.discover_session(&cfg, dir.path(), Utc::now()).is_err());
            assert!(agent.normalize_hook("Stop", serde_json::json!({})).is_none());
            assert!(agent.parse_transcript_line("{}", uuid::Uuid::new_v4(), TaskId::new(), &model).is_none());
            let store = Store::open_in_memory().unwrap();
            assert!(agent.probe(&cfg, &store).unwrap().is_none());
            assert!(agent.auth_status("/definitely/not/a/binary").is_err());
        }
    }

    #[test]
    fn auth_status_from_scripts() {
        if which::which("sh").is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("codex-ok");
        std::fs::write(&ok, "#!/bin/sh\necho 'Logged in using ChatGPT'\n").unwrap();
        let no = dir.path().join("codex-no");
        std::fs::write(&no, "#!/bin/sh\necho 'Not logged in' >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in [&ok, &no] {
                std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let a = agent_for(Provider::Codex).auth_status(&ok.to_string_lossy()).unwrap();
        assert!(a.logged_in);
        assert_eq!(a.detail, "Logged in using ChatGPT");
        let a = agent_for(Provider::Codex).auth_status(&no.to_string_lossy()).unwrap();
        assert!(!a.logged_in);
        assert_eq!(a.detail, "Not logged in");
        let a = agent_for(Provider::Gemini).auth_status(&ok.to_string_lossy()).unwrap();
        assert!(a.logged_in);
        assert!(a.detail.contains("login state not verifiable"), "{}", a.detail);
        assert_eq!(first_line(b"", b"", "fallback"), "fallback");
    }
}
