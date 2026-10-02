//! Build everything a session needs on disk, then start it in tmux.
//!
//! Per task we write `<state>/tasks/<task-id>/`:
//! * `prompt.md`     – the task brief the agent receives as its first message
//! * `settings.json` – (Claude) hooks that call back into `powerqueue hook`
//! * `launch.sh`     – the exact command line (kept for diagnostics; re-run to resume)
//! * `env`           – environment variables (0600)
//!
//! The provider-specific parts (extra files, env, argv, transcript path) come
//! from [`crate::session::agent::AgentCli::prepare`]; this module owns the
//! provider-neutral files and the tmux window. Resuming after a crash reuses
//! the same session id (`--resume` for Claude Code).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::{ClaudeConfig, Config};
use crate::domain::{
    BLOCKED_MARKER, DONE_MARKER, HookEvent, ModelTier, Provider, Session, SessionState, Task, TaskId, TaskSource,
};
use crate::paths::Paths;
pub use crate::session::agent::prompt_arg;
use crate::session::agent::{LaunchContext, agent_for};
use crate::tmux::{Tmux, shell_quote};
use crate::worktree::branch_name;

/// Tool rules every session gets so the completion protocol never waits on a
/// permission prompt. Users extend the list with `claude.allowed_tools`.
pub const ALWAYS_ALLOWED_TOOLS: [&str; 1] = ["Bash(powerqueue task *)"];

/// Files written for a launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPlan {
    pub task_dir: PathBuf,
    pub prompt_path: PathBuf,
    /// `settings.json` (Claude Code hooks); other providers may not write it.
    pub settings_path: PathBuf,
    pub script_path: PathBuf,
    /// The shell command tmux runs (`bash launch.sh`).
    pub shell_command: String,
    pub resume: bool,
    /// Which CLI the plan launches.
    #[serde(default = "default_provider")]
    pub provider: Provider,
    /// Where the CLI will write its transcript, when known at launch time.
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    /// Scan the transcript for the DONE / BLOCKED markers as a fallback
    /// completion signal.
    #[serde(default)]
    pub poll_transcript_for_completion: bool,
}

fn default_provider() -> Provider {
    Provider::Claude
}

/// Hooks that only record activity and may run in the background.
const ASYNC_HOOKS: [HookEvent; 4] =
    [HookEvent::SessionStart, HookEvent::Notification, HookEvent::PreCompact, HookEvent::UserPromptSubmit];

/// Seconds a hook may take before Claude Code gives up on it.
const HOOK_TIMEOUT_SECS: u64 = 10;

/// Compose the first user message for a task.
///
/// Contains the key and title, the description, the Linear link, the
/// worktree/branch rules and the completion protocol (the `powerqueue task
/// complete|block` commands plus the [`DONE_MARKER`] / [`BLOCKED_MARKER`]
/// lines). Attempts after the first also say how the previous one ended.
/// `cfg.claude.append_system_prompt` is deliberately *not* included here; it
/// goes to `--append-system-prompt`.
pub fn build_prompt(task: &Task, cfg: &Config, provider: Provider, attempt: u32, previous_error: Option<&str>) -> String {
    let branch = task.branch.clone().unwrap_or_else(|| branch_name(&cfg.repo.branch_template, &task.slug(), &task.id.short()));
    let mut p = String::new();
    let _ = writeln!(p, "# {}: {}", task.key, task.title.trim());
    p.push('\n');
    let description = task.description.trim();
    p.push_str(if description.is_empty() { "(no description provided)" } else { description });
    p.push('\n');
    if let TaskSource::Linear { url, identifier, .. } = &task.source
        && !url.is_empty()
    {
        let _ = write!(p, "\nSource: {identifier} <{url}>\n");
    }
    let commit_rule = if agent_for(provider).commits_in_session(cfg) {
        "- Commit as you go with clear messages.\n"
    } else {
        "- Do not run `git commit` or other git write commands: this session's sandbox keeps git metadata read-only. \
         Leave your changes in the working tree; powerqueue commits and pushes them when you complete the task.\n"
    };
    let _ = write!(
        p,
        "\n## Working rules\n\n\
         - You are working in a dedicated git worktree on branch `{branch}`: never switch branches and never touch other worktrees.\n\
         {commit_rule}\
         - Do not push unless asked; powerqueue pushes on completion.\n"
    );
    let _ = write!(
        p,
        "\n## Completion protocol\n\n\
         When the task is fully done, run `powerqueue task complete {id} --summary \"...\"` and then print `{DONE_MARKER}` as the last line of your final message.\n\
         If you are blocked and need a human, run `powerqueue task block {id} --reason \"...\"` and print `{BLOCKED_MARKER}`.\n",
        id = task.id,
    );
    if attempt > 1 {
        let _ = write!(p, "\n## Attempt {attempt}\n\nThis is attempt {attempt}");
        match previous_error.map(str::trim).filter(|e| !e.is_empty()) {
            Some(err) => {
                let _ = writeln!(p, "; the previous attempt ended with: {err}");
            }
            None => {
                p.push_str(". The previous attempt did not finish; check `git log` and the working tree before continuing.\n")
            }
        }
    }
    p
}

/// Claude Code settings JSON wiring every relevant hook to `powerqueue hook`.
///
/// Each event runs `<bin> hook --task <id> --session <sid> --event <Event>`
/// with a 10 s timeout. Activity-only events (`SessionStart`, `Notification`,
/// `PreCompact`, `UserPromptSubmit`) are `async`; `Stop`, `StopFailure` and
/// `SessionEnd` run synchronously so they are recorded before Claude moves
/// on. The settings carry hooks only: environment goes into `launch.sh`.
pub fn hook_settings(
    powerqueue_bin: &Path,
    task_id: TaskId,
    session_id: uuid::Uuid,
    _claude: &ClaudeConfig,
) -> serde_json::Value {
    let bin = shell_quote(&powerqueue_bin.to_string_lossy());
    let mut hooks = serde_json::Map::new();
    for event in HookEvent::ALL {
        let command = format!("{bin} hook --task {task_id} --session {session_id} --event {}", event.as_str());
        let mut hook = json!({ "type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECS });
        if ASYNC_HOOKS.contains(&event) {
            hook["async"] = json!(true);
        }
        hooks.insert(event.as_str().to_string(), json!([{ "hooks": [hook] }]));
    }
    json!({ "hooks": hooks })
}

/// Starts and restarts sessions.
#[derive(Debug, Clone)]
pub struct Launcher {
    pub paths: Paths,
    pub tmux: Tmux,
    /// Absolute path of the running `powerqueue` binary (for hooks).
    pub self_bin: PathBuf,
}

impl Launcher {
    pub fn new(paths: Paths, tmux: Tmux) -> Result<Self> {
        let self_bin = std::env::current_exe().context("cannot determine the powerqueue binary path")?;
        Ok(Self { paths, tmux, self_bin })
    }

    /// Write prompt, the provider's files, env and launch script. `resume` =
    /// reuse the session id (after a crash) instead of starting fresh; for
    /// CLIs with their own ids the previous session's `agent_session_id` is
    /// passed through `resume_id`. Fails when the task has no worktree yet
    /// or the provider cannot describe the launch (e.g. an invalid mode).
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &self,
        cfg: &Config,
        task: &Task,
        session_id: uuid::Uuid,
        model: &ModelTier,
        attempt: u32,
        resume: bool,
        previous_error: Option<&str>,
    ) -> Result<LaunchPlan> {
        self.prepare_with_resume_id(cfg, task, session_id, model, attempt, resume, None, previous_error)
    }

    /// [`Launcher::prepare`] with an explicit provider session id to resume.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_with_resume_id(
        &self,
        cfg: &Config,
        task: &Task,
        session_id: uuid::Uuid,
        model: &ModelTier,
        attempt: u32,
        resume: bool,
        resume_id: Option<&str>,
        previous_error: Option<&str>,
    ) -> Result<LaunchPlan> {
        let worktree = worktree_of(task)?;
        let provider = model.provider();
        let agent = agent_for(provider);
        let task_dir = self.paths.task_dir(&task.id.to_string());
        std::fs::create_dir_all(&task_dir).with_context(|| format!("cannot create {}", task_dir.display()))?;
        let prompt_path = task_dir.join("prompt.md");
        let settings_path = task_dir.join("settings.json");
        let env_path = task_dir.join("env");
        let script_path = task_dir.join("launch.sh");

        std::fs::write(&prompt_path, build_prompt(task, cfg, provider, attempt, previous_error))
            .with_context(|| format!("cannot write {}", prompt_path.display()))?;

        let own_id = session_id.to_string();
        let resume_ref = if resume { Some(resume_id.unwrap_or(own_id.as_str())) } else { None };
        let ctx = LaunchContext {
            cfg,
            paths: &self.paths,
            task,
            session_id,
            model,
            attempt,
            resume: resume_ref,
            task_dir: &task_dir,
            prompt_path: &prompt_path,
            worktree,
            self_bin: &self.self_bin,
        };
        let launch = agent.prepare(&ctx).with_context(|| format!("prepare {provider} launch for {}", task.key))?;
        for (path, contents, mode) in &launch.files {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
            }
            std::fs::write(path, contents).with_context(|| format!("cannot write {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode))
                    .with_context(|| format!("cannot chmod {}", path.display()))?;
            }
            #[cfg(not(unix))]
            let _ = mode;
        }

        let env = launch.env;
        let env_text = env.iter().fold(String::new(), |mut acc, (k, v)| {
            let _ = writeln!(acc, "{k}={v}");
            acc
        });
        crate::config::write_private(&env_path, env_text.as_bytes())
            .with_context(|| format!("cannot write {}", env_path.display()))?;

        let argv = launch.argv;
        let script = launch_script(task, attempt, worktree, &env, &argv, &self.self_bin);
        // The script exports `claude.env`, which may hold secrets: owner-only.
        crate::config::write_private(&script_path, script.as_bytes())
            .with_context(|| format!("cannot write {}", script_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("cannot chmod {}", script_path.display()))?;
        }

        tracing::info!(task = %task.key, session = %session_id, attempt, resume, model = %model, provider = %provider, dir = %task_dir.display(), "prepared launch files");
        Ok(LaunchPlan {
            shell_command: format!("sh {}", shell_quote(&script_path.to_string_lossy())),
            task_dir,
            prompt_path,
            settings_path,
            script_path,
            resume,
            provider,
            transcript_path: launch.transcript_path,
            poll_transcript_for_completion: launch.poll_transcript_for_completion,
        })
    }

    /// Run the provider's pre-launch step (trust seeding for Claude Code,
    /// the info/exclude entry for Antigravity's hook file),
    /// create the tmux window and return the new [`Session`] record.
    pub fn launch(
        &self,
        cfg: &Config,
        task: &Task,
        plan: &LaunchPlan,
        session_id: uuid::Uuid,
        model: &ModelTier,
        attempt: u32,
    ) -> Result<Session> {
        let worktree = worktree_of(task)?;
        agent_for(plan.provider)
            .pre_launch(cfg, &cfg.repo_path(), worktree)
            .with_context(|| format!("pre-launch step of {} for {}", plan.provider, task.key))?;
        let tmux_session = cfg.tmux.session_name.clone();
        self.tmux.ensure_session(&tmux_session, &cfg.repo_path())?;
        let window = self.tmux.new_window(&tmux_session, &task.slug(), worktree, &plan.shell_command, cfg.tmux.remain_on_exit)?;
        let now = Utc::now();
        tracing::info!(
            task = %task.key,
            session = %session_id,
            attempt,
            model = %model,
            resume = plan.resume,
            window = %window.window_id,
            pane = %window.pane_id,
            pid = window.pane_pid,
            provider = %plan.provider,
            "launched agent session"
        );
        Ok(Session {
            id: session_id,
            task_id: task.id,
            attempt,
            model: model.clone(),
            state: SessionState::Launching,
            tmux_session,
            tmux_window: window.window_id,
            pane_id: Some(window.pane_id),
            pid: Some(window.pane_pid),
            transcript_path: plan.transcript_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            exit_code: None,
            started_at: now,
            ended_at: None,
            last_activity_at: now,
            error: None,
            agent_session_id: None,
        })
    }

    /// Full command line (for `task show` and debugging).
    ///
    /// The last element is the prompt, expressed as the shell snippet
    /// `$(cat '<prompt_path>')` so the script (and a human re-running it)
    /// reads the prompt from disk. Everything before it is a literal argument.
    pub fn claude_command(
        cfg: &Config,
        model: &ModelTier,
        session_id: uuid::Uuid,
        settings_path: &Path,
        prompt_path: &Path,
        resume: bool,
        name: &str,
    ) -> Vec<String> {
        let c = &cfg.claude;
        let mut argv = vec![c.binary.clone()];
        argv.push(if resume { "--resume" } else { "--session-id" }.to_string());
        argv.push(session_id.to_string());
        argv.extend(["--model".to_string(), model.alias().to_string()]);
        argv.extend(["--permission-mode".to_string(), c.permission_mode.clone()]);
        argv.extend(["--settings".to_string(), settings_path.to_string_lossy().to_string()]);
        argv.extend(["--name".to_string(), name.to_string()]);
        if let Some(effort) = c.effort.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            argv.extend(["--effort".to_string(), effort.to_string()]);
        }
        if !c.fallback_models.is_empty() {
            argv.extend(["--fallback-model".to_string(), c.fallback_models.join(",")]);
        }
        let mut allowed: Vec<String> = c.allowed_tools.clone();
        for always in ALWAYS_ALLOWED_TOOLS {
            if !allowed.iter().any(|a| a == always) {
                allowed.push(always.to_string());
            }
        }
        argv.extend(["--allowedTools".to_string(), allowed.join(",")]);
        if let Some(text) = c.append_system_prompt.as_deref().filter(|t| !t.trim().is_empty()) {
            argv.extend(["--append-system-prompt".to_string(), text.to_string()]);
        }
        argv.extend(c.extra_args.iter().cloned());
        argv.push(prompt_arg(prompt_path));
        argv
    }
}

fn worktree_of(task: &Task) -> Result<&Path> {
    match task.worktree_path.as_deref() {
        Some(p) if !p.trim().is_empty() => Ok(Path::new(p)),
        _ => bail!("task {} has no worktree yet; create it before launching a session", task.key),
    }
}

/// Render `launch.sh`.
fn launch_script(
    task: &Task,
    attempt: u32,
    worktree: &Path,
    env: &[(String, String)],
    argv: &[String],
    self_bin: &Path,
) -> String {
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    let _ = writeln!(s, "# Generated by powerqueue for task {} (attempt {}). Re-run to resume by hand.", task.key, attempt);
    s.push_str("set -u\n");
    let _ = writeln!(s, "cd {} || exit 1", shell_quote(&worktree.to_string_lossy()));
    // `powerqueue task complete` must work inside the session even when the
    // binary is not on the user's PATH (e.g. a cargo build in a checkout).
    if let Some(dir) = self_bin.parent() {
        let _ = writeln!(s, "export PATH={}:\"$PATH\"", shell_quote(&dir.to_string_lossy()));
    }
    for (k, v) in env {
        let _ = writeln!(s, "export {k}={}", shell_quote(v));
    }
    let (prompt, args) = argv.split_last().expect("argv always ends with the prompt argument");
    let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    let _ = writeln!(s, "exec {} \"{prompt}\"", quoted.join(" "));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskSource;

    fn task() -> Task {
        let mut t = Task::new(
            "ENG-123",
            "Fix the flaky login test",
            TaskSource::Linear {
                issue_id: "lin-uuid".into(),
                identifier: "ENG-123".into(),
                url: "https://linear.app/acme/issue/ENG-123".into(),
                team_key: "ENG".into(),
            },
        );
        t.description = "The test fails every third run.\n\nSee CI logs.".into();
        t.branch = Some("pq/eng-123".into());
        t.worktree_path = Some("/work/wt/eng-123".into());
        t
    }

    fn config() -> Config {
        let mut cfg = Config::default();
        cfg.repo.path = "/work/repo".into();
        cfg
    }

    #[test]
    fn sandboxed_codex_sessions_are_told_not_to_commit() {
        let t = task();
        let mut cfg = config();
        let p = build_prompt(&t, &cfg, Provider::Codex, 1, None);
        assert!(p.contains("Do not run `git commit`"), "{p}");
        assert!(!p.contains("Commit as you go"), "{p}");
        cfg.codex.approval = "yolo".into();
        let p = build_prompt(&t, &cfg, Provider::Codex, 1, None);
        assert!(p.contains("Commit as you go"), "{p}");
        let p = build_prompt(&t, &cfg, Provider::Claude, 1, None);
        assert!(p.contains("Commit as you go"), "{p}");
    }

    #[test]
    fn prompt_contains_the_essentials() {
        let t = task();
        let cfg = config();
        let p = build_prompt(&t, &cfg, Provider::Claude, 1, None);
        assert!(p.starts_with("# ENG-123: Fix the flaky login test\n"), "{p}");
        assert!(p.contains("The test fails every third run."));
        assert!(p.contains("https://linear.app/acme/issue/ENG-123"));
        assert!(p.contains("branch `pq/eng-123`"));
        assert!(p.contains(&format!("powerqueue task complete {} --summary", t.id)));
        assert!(p.contains(&format!("powerqueue task block {} --reason", t.id)));
        assert!(p.contains(DONE_MARKER));
        assert!(p.contains(BLOCKED_MARKER));
        assert!(!p.contains("attempt"), "first attempt has no attempt section: {p}");
    }

    #[test]
    fn prompt_mentions_previous_attempt_and_derives_branch() {
        let mut t = task();
        t.branch = None;
        t.source = TaskSource::Manual;
        t.description = String::new();
        let mut cfg = config();
        cfg.claude.append_system_prompt = Some("SECRET SYSTEM PROMPT".into());
        let p = build_prompt(&t, &cfg, Provider::Claude, 3, Some("pane died with exit 137"));
        assert!(p.contains("This is attempt 3; the previous attempt ended with: pane died with exit 137"), "{p}");
        assert!(p.contains("branch `pq/eng-123`"));
        assert!(p.contains("(no description provided)"));
        assert!(!p.contains("linear.app"));
        assert!(!p.contains("SECRET SYSTEM PROMPT"));
        let p = build_prompt(&t, &cfg, Provider::Claude, 2, None);
        assert!(p.contains("This is attempt 2. The previous attempt did not finish"), "{p}");
    }

    #[test]
    fn hook_settings_cover_all_events() {
        let t = task();
        let sid = uuid::Uuid::new_v4();
        let v = hook_settings(Path::new("/opt/power queue/bin/powerqueue"), t.id, sid, &ClaudeConfig::default());
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 7);
        for event in HookEvent::ALL {
            let entry = &hooks[event.as_str()][0]["hooks"][0];
            assert_eq!(entry["type"], "command");
            assert_eq!(entry["timeout"], 10);
            let cmd = entry["command"].as_str().unwrap();
            assert_eq!(
                cmd,
                format!("'/opt/power queue/bin/powerqueue' hook --task {} --session {sid} --event {}", t.id, event.as_str()),
                "{event:?}"
            );
            let is_async = entry.get("async").and_then(|a| a.as_bool()).unwrap_or(false);
            let expected = matches!(
                event,
                HookEvent::SessionStart | HookEvent::Notification | HookEvent::PreCompact | HookEvent::UserPromptSubmit
            );
            assert_eq!(is_async, expected, "{event:?}");
        }
        assert!(v.get("env").is_none());
    }

    #[test]
    fn claude_command_ordering() {
        let mut cfg = config();
        cfg.claude.effort = Some("high".into());
        cfg.claude.fallback_models = vec!["sonnet".into(), "haiku".into()];
        cfg.claude.allowed_tools = vec!["Bash(git:*)".into(), "Read".into()];
        cfg.claude.append_system_prompt = Some("Be terse.".into());
        cfg.claude.extra_args = vec!["--add-dir".into(), "/shared".into()];
        let sid = uuid::Uuid::new_v4();
        let argv = Launcher::claude_command(
            &cfg,
            &ModelTier::opus(),
            sid,
            Path::new("/s/settings.json"),
            Path::new("/s/prompt.md"),
            false,
            "ENG-123",
        );
        let sid_s = sid.to_string();
        let expected = vec![
            "claude",
            "--session-id",
            sid_s.as_str(),
            "--model",
            "opus",
            "--permission-mode",
            "acceptEdits",
            "--settings",
            "/s/settings.json",
            "--name",
            "ENG-123",
            "--effort",
            "high",
            "--fallback-model",
            "sonnet,haiku",
            "--allowedTools",
            "Bash(git:*),Read,Bash(powerqueue task *)",
            "--append-system-prompt",
            "Be terse.",
            "--add-dir",
            "/shared",
            "$(cat /s/prompt.md)",
        ];
        assert_eq!(argv, expected);

        let cfg = config();
        let argv = Launcher::claude_command(
            &cfg,
            &ModelTier::sonnet(),
            sid,
            Path::new("/s/settings.json"),
            Path::new("/s/prompt.md"),
            true,
            "x",
        );
        assert_eq!(argv[1], "--resume");
        assert_eq!(argv[2], sid.to_string());
        assert_eq!(argv.len(), 14, "{argv:?}");
        assert!(!argv.iter().any(|a| a == "--effort" || a == "--fallback-model" || a == "--append-system-prompt"));
        let allowed = argv.iter().position(|a| a == "--allowedTools").expect("completion commands are always allowed");
        assert_eq!(argv[allowed + 1], "Bash(powerqueue task *)");
    }

    #[test]
    fn prepare_writes_the_four_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let launcher = Launcher { paths, tmux: Tmux::new("tmux", None), self_bin: PathBuf::from("/usr/local/bin/powerqueue") };
        let mut cfg = config();
        cfg.claude.env.insert("ANTHROPIC_SMALL_FAST_MODEL".into(), "haiku".into());
        cfg.claude.env.insert("TRICKY".into(), "it's $HOME".into());
        let mut t = task();
        t.worktree_path = Some(dir.path().join("wt").to_string_lossy().to_string());
        let sid = uuid::Uuid::new_v4();

        let plan = launcher.prepare(&cfg, &t, sid, &ModelTier::fable(), 2, true, Some("crashed")).unwrap();
        assert_eq!(plan.task_dir, dir.path().join("state/tasks").join(t.id.to_string()));
        assert!(plan.resume);
        assert_eq!(plan.provider, Provider::Claude);
        assert!(plan.transcript_path.as_ref().unwrap().to_string_lossy().ends_with(&format!("{sid}.jsonl")));
        assert!(!plan.poll_transcript_for_completion);
        assert_eq!(plan.shell_command, format!("sh {}", shell_quote(&plan.script_path.to_string_lossy())));

        let prompt = std::fs::read_to_string(&plan.prompt_path).unwrap();
        assert!(prompt.contains("# ENG-123") && prompt.contains("attempt 2"));

        let settings: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&plan.settings_path).unwrap()).unwrap();
        assert_eq!(settings["hooks"].as_object().unwrap().len(), 7);
        assert!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .starts_with("/usr/local/bin/powerqueue hook --task")
        );

        let env = std::fs::read_to_string(plan.task_dir.join("env")).unwrap();
        assert!(env.contains("ANTHROPIC_SMALL_FAST_MODEL=haiku\n"));
        assert!(env.contains(&format!("POWERQUEUE_TASK_ID={}\n", t.id)));
        assert!(env.contains("POWERQUEUE_TASK_KEY=ENG-123\n"));
        assert!(env.contains(&format!("POWERQUEUE_SESSION_ID={sid}\n")));

        let script = std::fs::read_to_string(&plan.script_path).unwrap();
        assert!(script.starts_with("#!/bin/sh\n"), "{script}");
        assert!(script.contains("set -u\n"));
        assert!(script.contains(&format!("cd {} || exit 1\n", shell_quote(&t.worktree_path.clone().unwrap()))));
        assert!(script.contains("export ANTHROPIC_SMALL_FAST_MODEL=haiku\n"));
        // The exact quoting style is shlex's choice; what matters is that `sh` sees the original value.
        let tricky_line = script.lines().find(|l| l.starts_with("export TRICKY=")).expect("TRICKY exported");
        #[cfg(unix)]
        {
            let out =
                std::process::Command::new("sh").arg("-c").arg(format!("{tricky_line}; printf %s \"$TRICKY\"")).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), "it's $HOME", "{tricky_line}");
        }
        assert!(script.contains("export POWERQUEUE_SESSION_ID="));
        let exec_line = script.lines().last().unwrap();
        assert!(exec_line.starts_with("exec claude --resume "), "{exec_line}");
        assert!(exec_line.contains("--model fable"));
        assert!(exec_line.contains("--name ENG-123"));
        assert!(exec_line.ends_with(&format!("\"$(cat {})\"", shell_quote(&plan.prompt_path.to_string_lossy()))), "{exec_line}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&plan.task_dir.join("env")), 0o600);
            assert_eq!(mode(&plan.script_path), 0o700);
        }

        // Re-preparing (fresh, attempt 1) overwrites in place.
        let plan2 = launcher.prepare(&cfg, &t, sid, &ModelTier::sonnet(), 1, false, None).unwrap();
        assert_eq!(plan2.task_dir, plan.task_dir);
        assert!(!plan2.resume);
        let script = std::fs::read_to_string(&plan2.script_path).unwrap();
        assert!(script.contains("exec claude --session-id "));
    }

    #[test]
    fn prepare_requires_a_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let launcher =
            Launcher { paths: Paths::rooted(dir.path()), tmux: Tmux::new("tmux", None), self_bin: PathBuf::from("/bin/pq") };
        let mut t = task();
        t.worktree_path = None;
        let err =
            launcher.prepare(&config(), &t, uuid::Uuid::new_v4(), &ModelTier::sonnet(), 1, false, None).unwrap_err().to_string();
        assert!(err.contains("no worktree"), "{err}");
        assert!(!dir.path().join("state/tasks").exists());
    }

    #[test]
    fn prepare_launches_other_providers() {
        let dir = tempfile::tempdir().unwrap();
        let launcher =
            Launcher { paths: Paths::rooted(dir.path()), tmux: Tmux::new("tmux", None), self_bin: PathBuf::from("/bin/pq") };
        let mut t = task();
        let wt = dir.path().join("wt");
        t.worktree_path = Some(wt.to_string_lossy().to_string());
        let mut cfg = config();
        cfg.codex.env.insert("CODEX_HOME".into(), "/x/codex".into());
        let sid = uuid::Uuid::new_v4();
        let plan = launcher.prepare(&cfg, &t, sid, &ModelTier::new("gpt-6.1-sol"), 1, false, None).unwrap();
        assert_eq!(plan.provider, Provider::Codex);
        assert_eq!(plan.transcript_path, None);
        let script = std::fs::read_to_string(&plan.script_path).unwrap();
        assert!(script.contains("export CODEX_HOME=/x/codex\n"), "{script}");
        let exec_line = script.lines().last().unwrap();
        assert!(exec_line.starts_with("exec codex -C "), "{exec_line}");
        assert!(exec_line.contains("-m gpt-6.1-sol"), "{exec_line}");

        // A crash resume passes the provider's own session id.
        let plan = launcher
            .prepare_with_resume_id(&cfg, &t, sid, &ModelTier::new("gpt-6.1-sol"), 2, true, Some("thread-1"), Some("crashed"))
            .unwrap();
        let script = std::fs::read_to_string(&plan.script_path).unwrap();
        assert!(script.lines().last().unwrap().starts_with("exec codex resume thread-1 -C "), "{script}");

        // Antigravity writes its hook file into the worktree (creating `.agents/`).
        let plan = launcher.prepare(&cfg, &t, sid, &ModelTier::new("gemini-3-pro"), 1, false, None).unwrap();
        assert_eq!(plan.provider, Provider::Gemini);
        assert!(plan.poll_transcript_for_completion);
        let hooks = std::fs::read_to_string(wt.join(".agents/hooks.json")).unwrap();
        assert!(hooks.contains("hook --provider gemini"), "{hooks}");
        let script = std::fs::read_to_string(&plan.script_path).unwrap();
        assert!(script.lines().last().unwrap().starts_with("exec agy --model gemini-3-pro"), "{script}");
    }
}
