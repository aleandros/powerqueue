//! Google Antigravity CLI (`agy`), the CLI behind Google AI Pro/Ultra
//! subscriptions. **Experimental.**
//!
//! unverified: see docs/reference/providers-research.md. Everything here
//! comes from vendor docs and community reports; `agy` was not available
//! where this was written. The shapes powerqueue relies on:
//!
//! * Launch: `agy --model <slug> <mode flag> [--effort <e>] --add-dir ...
//!   -i "<prompt>"` in the worktree (`launch.sh` `cd`s there first); resume
//!   with `--conversation <id>`.
//! * Completion: a `Stop` hook in `<worktree>/.agents/hooks.json` calling
//!   `powerqueue hook --provider gemini ... --event Stop`; its stdin carries
//!   `transcript_path`. As a fallback the daemon polls the transcript for
//!   the DONE / BLOCKED markers ([`AgentCli::polls_transcript_for_completion`]).
//! * Discovery: `~/.gemini/antigravity-cli/cache/last_conversations.json`
//!   maps workspace path → conversation id; the transcript lives at
//!   `brain/<id>/.system_generated/logs/transcript.jsonl` with lines
//!   `{step_index, source, type, status, created_at, content}`. It carries
//!   no token counts, so no usage is recorded from it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};

use crate::budget::ObservedUsage;
use crate::config::{Config, GEMINI_MODES};
use crate::domain::{HookEvent, ModelTier, Provider, TaskId, UsageRecord};
use crate::session::agent::{
    AgentCli, AgentLaunch, AuthStatus, LaunchContext, canonical, first_line, home_dir, matches_signature, prompt_arg, session_env,
};
use crate::session::transcript::TranscriptState;
use crate::store::Store;
use crate::tmux::shell_quote;

/// Lower-case rate-limit signatures. unverified: see docs/reference/providers-research.md.
/// (A bare `quota` would match ordinary prose, so the phrases are kept specific.)
const SIGNATURES: [&str; 5] = ["resource_exhausted", "individual quota reached", "quota reached", "quota exceeded", "code 429"];

/// Transcript step type of the agent's answers.
const PLANNER_RESPONSE: &str = "PLANNER_RESPONSE";

/// Marker in hook commands powerqueue writes (to replace them on relaunch).
const HOOK_MARKER: &str = " hook --provider gemini ";

/// Slack between the CLI writing its state and the session row's start time.
const DISCOVERY_SLACK: Duration = Duration::seconds(10);

/// Google Antigravity CLI (`agy`). Experimental.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiCli;

/// Where `agy` keeps its state: `POWERQUEUE_AGY_HOME` (from `gemini.env`,
/// then the daemon's environment; a powerqueue-only override), else
/// `~/.gemini/antigravity-cli`.
pub fn agy_home(cfg: &Config) -> PathBuf {
    if let Some(dir) = cfg.gemini.env.get("POWERQUEUE_AGY_HOME").filter(|d| !d.trim().is_empty()) {
        return crate::paths::expand_tilde(dir);
    }
    if let Some(dir) = std::env::var_os("POWERQUEUE_AGY_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    home_dir().join(".gemini").join("antigravity-cli")
}

/// The transcript of conversation `id` under `home`.
pub fn transcript_path(home: &Path, id: &str) -> PathBuf {
    home.join("brain").join(id).join(".system_generated").join("logs").join("transcript.jsonl")
}

/// Flags for `gemini.mode` (see [`GEMINI_MODES`]).
pub fn mode_flags(mode: &str) -> Result<Vec<String>> {
    let flags: &[&str] = match mode {
        "skip-permissions" => &["--dangerously-skip-permissions"],
        "accept-edits" => &["--mode", "accept-edits"],
        "plan" => &["--mode", "plan"],
        other => bail!("gemini.mode `{other}` is not one of {}", GEMINI_MODES.join("|")),
    };
    Ok(flags.iter().map(|s| s.to_string()).collect())
}

/// Full `agy` command line (last element: the prompt, after `-i`).
pub fn agy_command(ctx: &LaunchContext<'_>) -> Result<Vec<String>> {
    let g = &ctx.cfg.gemini;
    let mut argv = vec![g.binary.clone()];
    if let Some(id) = ctx.resume {
        argv.extend(["--conversation".to_string(), id.to_string()]);
    }
    argv.extend(["--model".to_string(), ctx.model.alias().to_string()]);
    argv.extend(mode_flags(&g.mode)?);
    if let Some(effort) = g.effort.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        argv.extend(["--effort".to_string(), effort.to_string()]);
    }
    for dir in [&ctx.paths.data_dir, &ctx.paths.state_dir] {
        argv.extend(["--add-dir".to_string(), dir.to_string_lossy().to_string()]);
    }
    argv.extend(g.extra_args.iter().cloned());
    argv.push("-i".to_string());
    argv.push(prompt_arg(ctx.prompt_path));
    Ok(argv)
}

/// `<worktree>/.agents/hooks.json` with powerqueue's `Stop` hook merged into
/// `existing` (the file as it is, if any). Earlier powerqueue entries are
/// replaced; other hooks are kept.
pub fn hooks_json(existing: Option<&str>, command: &str) -> Result<String> {
    let mut doc: serde_json::Value = match existing.map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => serde_json::from_str(text).context("parse existing .agents/hooks.json")?,
        None => serde_json::json!({}),
    };
    if !doc.is_object() {
        bail!(".agents/hooks.json is not a JSON object");
    }
    let hooks = doc.as_object_mut().expect("checked above").entry("hooks").or_insert_with(|| serde_json::json!({}));
    let Some(hooks) = hooks.as_object_mut() else { bail!("`hooks` in .agents/hooks.json is not an object") };
    let stop = hooks.entry("Stop").or_insert_with(|| serde_json::json!([]));
    let Some(stop) = stop.as_array_mut() else { bail!("`hooks.Stop` in .agents/hooks.json is not an array") };
    stop.retain(|entry| {
        !entry["hooks"]
            .as_array()
            .is_some_and(|hs| hs.iter().any(|h| h["command"].as_str().is_some_and(|c| c.contains(HOOK_MARKER))))
    });
    stop.push(serde_json::json!({ "hooks": [{ "type": "command", "command": command }] }));
    Ok(serde_json::to_string_pretty(&doc).context("serialise .agents/hooks.json")? + "\n")
}

/// The hook command for a session.
pub fn stop_hook_command(self_bin: &Path, task_id: TaskId, session_id: uuid::Uuid) -> String {
    format!(
        "{} hook --provider gemini --task {task_id} --session {session_id} --event Stop",
        shell_quote(&self_bin.to_string_lossy())
    )
}

/// Append `.agents/` to `<git dir>/info/exclude` unless it is already there.
/// Returns whether the file changed.
pub fn ensure_excluded(git_dir: &Path, pattern: &str) -> Result<bool> {
    let file = git_dir.join("info").join("exclude");
    let current = std::fs::read_to_string(&file).unwrap_or_default();
    if current.lines().any(|l| l.trim() == pattern) {
        return Ok(false);
    }
    std::fs::create_dir_all(git_dir.join("info")).with_context(|| format!("create {}", git_dir.join("info").display()))?;
    let mut text = current;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(pattern);
    text.push('\n');
    std::fs::write(&file, text).with_context(|| format!("write {}", file.display()))?;
    Ok(true)
}

/// The text of a transcript step's `content` (a string, or an object with `text`).
fn content_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("content")?;
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => other.get("text").and_then(|t| t.as_str()).map(str::to_string).or_else(|| Some(other.to_string())),
    }
}

/// The content of the last `PLANNER_RESPONSE` step in a transcript file.
pub fn last_planner_response(transcript: &Path) -> Option<String> {
    let text = std::fs::read_to_string(transcript).ok()?;
    text.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("type").and_then(|t| t.as_str()) == Some(PLANNER_RESPONSE))
        .and_then(|v| content_text(&v))
}

/// The conversation id for `worktree` in `last_conversations.json`.
fn conversation_for(map: &serde_json::Value, worktree: &Path) -> Option<String> {
    let physical = canonical(worktree);
    for key in [worktree.to_string_lossy(), physical.to_string_lossy()] {
        let key = key.trim_end_matches('/');
        let Some(v) = map.get(key).or_else(|| map.get(format!("{key}/"))) else { continue };
        let id = v
            .as_str()
            .or_else(|| v.get("id").and_then(|i| i.as_str()))
            .or_else(|| v.get("conversation_id").and_then(|i| i.as_str()));
        if let Some(id) = id.filter(|i| !i.trim().is_empty()) {
            return Some(id.to_string());
        }
    }
    None
}

impl AgentCli for GeminiCli {
    fn provider(&self) -> Provider {
        Provider::Gemini
    }

    // unverified: see docs/reference/providers-research.md
    fn prepare(&self, ctx: &LaunchContext<'_>) -> Result<AgentLaunch> {
        let hooks_path = ctx.worktree.join(".agents").join("hooks.json");
        let existing = std::fs::read_to_string(&hooks_path).ok();
        let command = stop_hook_command(ctx.self_bin, ctx.task.id, ctx.session_id);
        let hooks = hooks_json(existing.as_deref(), &command).or_else(|e| {
            tracing::warn!(path = %hooks_path.display(), error = %format!("{e:#}"), "replacing unreadable .agents/hooks.json");
            hooks_json(None, &command)
        })?;
        let transcript = ctx.resume.map(|id| transcript_path(&agy_home(ctx.cfg), id));
        Ok(AgentLaunch {
            files: vec![(hooks_path, hooks, 0o644)],
            env: session_env(&ctx.cfg.gemini.env, ctx.task, ctx.session_id),
            argv: agy_command(ctx)?,
            transcript_path: transcript,
            poll_transcript_for_completion: true,
        })
    }

    /// Keep the generated `.agents/` out of `git status` (and out of the
    /// agent's commits) through the repository's `info/exclude`.
    fn pre_launch(&self, _cfg: &Config, repo: &Path, _worktree: &Path) -> Result<()> {
        let git_dir = repo.join(".git");
        if !git_dir.is_dir() {
            tracing::debug!(repo = %repo.display(), "no .git directory; not excluding .agents/");
            return Ok(());
        }
        match ensure_excluded(&git_dir, ".agents/") {
            Ok(true) => tracing::info!(repo = %repo.display(), "added .agents/ to .git/info/exclude"),
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "could not exclude .agents/; it may show up in git status"),
        }
        Ok(())
    }

    // unverified: see docs/reference/providers-research.md
    fn discover_session(&self, cfg: &Config, worktree: &Path, started_after: DateTime<Utc>) -> Result<Option<(String, PathBuf)>> {
        let home = agy_home(cfg);
        let file = home.join("cache").join("last_conversations.json");
        let Ok(meta) = std::fs::metadata(&file) else { return Ok(None) };
        let modified = meta.modified().map(DateTime::<Utc>::from).unwrap_or(started_after);
        if modified < started_after - DISCOVERY_SLACK {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
        let map: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(file = %file.display(), error = %e, "last_conversations.json is not JSON (yet)");
                return Ok(None);
            }
        };
        Ok(conversation_for(&map, worktree).map(|id| {
            let path = transcript_path(&home, &id);
            (id, path)
        }))
    }

    /// agy transcripts carry no token counts.
    fn parse_transcript_line(
        &self,
        _line: &str,
        _session_id: uuid::Uuid,
        _task_id: TaskId,
        _launched_model: &ModelTier,
    ) -> Option<UsageRecord> {
        None
    }

    // unverified: see docs/reference/providers-research.md
    fn observe_transcript_line(&self, line: &str, state: &mut TranscriptState) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
        let Some(text) = content_text(&v) else { return };
        if v.get("type").and_then(|t| t.as_str()) == Some(PLANNER_RESPONSE) {
            state.pending_message = Some(text);
        } else if v.get("status").and_then(|s| s.as_str()).is_some_and(|s| s.to_ascii_uppercase().contains("ERROR"))
            && matches_signature(&text, &SIGNATURES)
        {
            state.rate_limit_errors.push(text);
        }
    }

    fn polls_transcript_for_completion(&self) -> bool {
        true
    }

    // unverified: see docs/reference/providers-research.md
    fn normalize_hook(&self, event: &str, payload: serde_json::Value) -> Option<(HookEvent, serde_json::Value)> {
        if !event.eq_ignore_ascii_case("Stop") {
            return None;
        }
        let mut payload = if payload.is_object() { payload } else { serde_json::json!({}) };
        let error = payload.get("error").and_then(|e| e.as_str()).map(str::to_string);
        if let Some(error) = error.filter(|e| matches_signature(e, &SIGNATURES)) {
            return Some((
                HookEvent::StopFailure,
                serde_json::json!({ "error_type": "rate_limit", "error": "rate_limit", "error_message": error }),
            ));
        }
        let message = payload
            .get("transcript_path")
            .and_then(|p| p.as_str())
            .and_then(|p| last_planner_response(Path::new(p)))
            .unwrap_or_default();
        payload["last_assistant_message"] = serde_json::Value::String(message);
        Some((HookEvent::Stop, payload))
    }

    fn rate_limit_signatures(&self) -> &'static [&'static str] {
        &SIGNATURES
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

    fn probe(&self, cfg: &Config, _store: &Store) -> Result<Option<ObservedUsage>> {
        crate::budget::UsageProbe::probe(&crate::budget::probes::gemini::GeminiProbe::new(&cfg.gemini.binary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Task, TaskSource};
    use crate::paths::Paths;
    use crate::session::agent::agent_for;

    #[test]
    fn prepare_builds_argv_and_hook_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.repo.path = dir.path().join("repo").display().to_string();
        cfg.gemini.env.insert("POWERQUEUE_AGY_HOME".into(), dir.path().join("agy").display().to_string());
        cfg.gemini.extra_args = vec!["--sandbox".into()];
        let wt = dir.path().join("wt");
        let mut t = Task::new("ENG-7", "Do the thing", TaskSource::Manual);
        t.worktree_path = Some(wt.display().to_string());
        let paths = Paths::rooted(&dir.path().join("home"));
        let sid = uuid::Uuid::new_v4();
        let model = ModelTier::new("gemini-3-pro");
        let prompt = dir.path().join("prompt.md");
        let ctx = LaunchContext {
            cfg: &cfg,
            paths: &paths,
            task: &t,
            session_id: sid,
            model: &model,
            attempt: 1,
            resume: None,
            task_dir: dir.path(),
            prompt_path: &prompt,
            worktree: &wt,
            self_bin: Path::new("/opt/p q/powerqueue"),
        };
        let launch = agent_for(Provider::Gemini).prepare(&ctx).unwrap();
        assert!(launch.poll_transcript_for_completion);
        assert_eq!(launch.transcript_path, None);
        let data = paths.data_dir.to_string_lossy().to_string();
        let state = paths.state_dir.to_string_lossy().to_string();
        let p = prompt_arg(&prompt);
        let expected = [
            "agy",
            "--model",
            "gemini-3-pro",
            "--dangerously-skip-permissions",
            "--effort",
            "high",
            "--add-dir",
            data.as_str(),
            "--add-dir",
            state.as_str(),
            "--sandbox",
            "-i",
            p.as_str(),
        ];
        assert_eq!(launch.argv, expected);
        assert_eq!(launch.files.len(), 1);
        assert_eq!(launch.files[0].0, wt.join(".agents/hooks.json"));
        let hooks: serde_json::Value = serde_json::from_str(&launch.files[0].1).unwrap();
        assert_eq!(
            hooks["hooks"]["Stop"][0]["hooks"][0]["command"],
            format!("'/opt/p q/powerqueue' hook --provider gemini --task {} --session {sid} --event Stop", t.id)
        );
        assert_eq!(hooks["hooks"]["Stop"][0]["hooks"][0]["type"], "command");

        let mut cfg2 = cfg.clone();
        cfg2.gemini.mode = "plan".into();
        cfg2.gemini.effort = None;
        let ctx = LaunchContext { cfg: &cfg2, resume: Some("conv-1"), ..ctx };
        let launch = agent_for(Provider::Gemini).prepare(&ctx).unwrap();
        assert_eq!(&launch.argv[..7], ["agy", "--conversation", "conv-1", "--model", "gemini-3-pro", "--mode", "plan"]);
        assert!(!launch.argv.contains(&"--effort".to_string()));
        assert_eq!(launch.transcript_path, Some(transcript_path(&dir.path().join("agy"), "conv-1")));
        assert_eq!(mode_flags("accept-edits").unwrap(), ["--mode", "accept-edits"]);
        assert!(mode_flags("yolo").is_err());
    }

    #[test]
    fn hooks_json_merges_and_replaces_our_entry() {
        let existing = r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"lint"}]}],"Stop":[{"hooks":[{"type":"command","command":"say done"}]},{"hooks":[{"type":"command","command":"/old/pq hook --provider gemini --task x --session y --event Stop"}]}]},"other":1}"#;
        let out: serde_json::Value = serde_json::from_str(
            &hooks_json(Some(existing), "/new/pq hook --provider gemini --task a --session b --event Stop").unwrap(),
        )
        .unwrap();
        assert_eq!(out["other"], 1);
        assert_eq!(out["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "lint");
        let stop = out["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "say done");
        assert!(stop[1]["hooks"][0]["command"].as_str().unwrap().starts_with("/new/pq"));
        assert!(hooks_json(Some("[1]"), "x").is_err());
        assert!(hooks_json(Some("{"), "x").is_err());
        let fresh: serde_json::Value = serde_json::from_str(&hooks_json(Some("  "), "cmd").unwrap()).unwrap();
        assert_eq!(fresh, serde_json::json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"cmd"}]}]}}));
    }

    #[test]
    fn pre_launch_excludes_agents_dir_once() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let gemini = agent_for(Provider::Gemini);
        std::fs::create_dir_all(&repo).unwrap();
        gemini.pre_launch(&Config::default(), &repo, &repo).unwrap(); // no .git: nothing happens
        std::fs::create_dir_all(repo.join(".git/info")).unwrap();
        std::fs::write(repo.join(".git/info/exclude"), "# git ls-files --others --exclude-from=.git/info/exclude\n*.log")
            .unwrap();
        gemini.pre_launch(&Config::default(), &repo, &repo).unwrap();
        gemini.pre_launch(&Config::default(), &repo, &repo).unwrap();
        let text = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches(".agents/").count(), 1, "{text}");
        assert!(text.contains("*.log\n.agents/\n"), "{text}");
    }

    #[test]
    fn discovers_conversation_and_reads_planner_responses() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("agy");
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let mut cfg = Config::default();
        cfg.gemini.env.insert("POWERQUEUE_AGY_HOME".into(), home.display().to_string());
        let gemini = agent_for(Provider::Gemini);
        let start = Utc::now();
        assert!(gemini.discover_session(&cfg, &wt, start).unwrap().is_none());
        std::fs::create_dir_all(home.join("cache")).unwrap();
        let map = serde_json::json!({ canonical(&wt).to_string_lossy(): "conv-9", "/elsewhere": "conv-1" });
        std::fs::write(home.join("cache/last_conversations.json"), map.to_string()).unwrap();
        let (id, path) = gemini.discover_session(&cfg, &wt, start).unwrap().unwrap();
        assert_eq!(id, "conv-9");
        assert_eq!(path, home.join("brain/conv-9/.system_generated/logs/transcript.jsonl"));
        assert!(gemini.discover_session(&cfg, &wt, start + Duration::hours(1)).unwrap().is_none(), "stale map ignored");
        assert!(gemini.discover_session(&cfg, &dir.path().join("nope"), start).unwrap().is_none());

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lines = [
            r#"{"step_index":0,"source":"USER","type":"USER_INPUT","status":"DONE","created_at":"2026-10-01T10:00:00Z","content":"do it"}"#,
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-01T10:00:05Z","content":"Working"}"#,
            r#"{"step_index":2,"source":"MODEL","type":"RUN_COMMAND","status":"DONE","content":{"command":"ls"}}"#,
            r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":{"text":"All done [[POWERQUEUE:DONE]] ok"}}"#,
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        assert_eq!(last_planner_response(&path).as_deref(), Some("All done [[POWERQUEUE:DONE]] ok"));

        let mut st = TranscriptState::default();
        for l in lines {
            gemini.observe_transcript_line(l, &mut st);
            assert!(gemini.parse_transcript_line(l, uuid::Uuid::nil(), TaskId::new(), &ModelTier::new("gemini-3-pro")).is_none());
        }
        assert_eq!(st.take_pending_message().as_deref(), Some("All done [[POWERQUEUE:DONE]] ok"));
        gemini.observe_transcript_line(
            r#"{"type":"ERROR_MESSAGE","status":"ERROR","content":"RESOURCE_EXHAUSTED (code 429): Individual quota reached."}"#,
            &mut st,
        );
        assert_eq!(st.take_rate_limit_errors().len(), 1);

        let (ev, p) = gemini
            .normalize_hook("Stop", serde_json::json!({ "session_id": "conv-9", "transcript_path": path.to_string_lossy() }))
            .unwrap();
        assert_eq!(ev, HookEvent::Stop);
        assert_eq!(p["last_assistant_message"], "All done [[POWERQUEUE:DONE]] ok");
        assert_eq!(p["session_id"], "conv-9");
        let (ev, p) =
            gemini.normalize_hook("Stop", serde_json::json!({ "error": "Individual quota reached. Resets in 2h" })).unwrap();
        assert_eq!(ev, HookEvent::StopFailure);
        assert_eq!(p["error_type"], "rate_limit");
        let (_, p) = gemini.normalize_hook("Stop", serde_json::json!(null)).unwrap();
        assert_eq!(p["last_assistant_message"], "");
        assert!(gemini.normalize_hook("PreToolUse", serde_json::json!({})).is_none());
    }

    #[test]
    fn auth_status_reports_version() {
        if which::which("sh").is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("agy");
        std::fs::write(&bin, "#!/bin/sh\necho 'agy 1.2.3'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let a = agent_for(Provider::Gemini).auth_status(&bin.to_string_lossy()).unwrap();
        assert!(a.logged_in);
        assert_eq!(a.detail, "agy 1.2.3 (login state not verifiable)");
        assert!(agent_for(Provider::Gemini).auth_status("/definitely/not/a/binary").is_err());
    }
}
