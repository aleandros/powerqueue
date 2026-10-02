//! Claude Code: `--session-id`, hook settings in a per-task `settings.json`,
//! workspace trust in `~/.claude.json`, JSONL transcripts under
//! `~/.claude/projects/<encoded cwd>/<session id>.jsonl`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::budget::ObservedUsage;
use crate::config::{CLAUDE_PERMISSION_MODES, Config};
use crate::domain::{HookEvent, ModelTier, Provider, TaskId, UsageRecord};
use crate::session::agent::{AgentCli, AgentLaunch, AuthStatus, LaunchContext, session_env};
use crate::store::Store;

/// Claude Code: `--session-id`, hook settings, JSONL transcripts.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeCli;

impl AgentCli for ClaudeCli {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn prepare(&self, ctx: &LaunchContext<'_>) -> Result<AgentLaunch> {
        use crate::session::launcher::{Launcher, hook_settings};
        let settings_path = ctx.task_dir.join("settings.json");
        let mut settings = hook_settings(ctx.self_bin, ctx.task.id, ctx.session_id, &ctx.cfg.claude);
        settings["statusLine"] = crate::budget::probes::claude::status_line_setting(ctx.self_bin, ctx.task.id, ctx.session_id);
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
            env: session_env(&ctx.cfg.claude.env, ctx.task, ctx.session_id),
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

    fn accepts_session_id(&self) -> bool {
        true
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

    fn probe(&self, _cfg: &Config, store: &Store) -> Result<Option<ObservedUsage>> {
        crate::budget::UsageProbe::probe(&crate::budget::probes::claude::ClaudeProbe::new(store.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Task, TaskSource};
    use crate::paths::Paths;
    use crate::session::agent::agent_for;

    fn task(dir: &Path) -> Task {
        let mut t = Task::new("ENG-7", "Do the thing", TaskSource::Manual);
        t.worktree_path = Some(dir.join("wt").display().to_string());
        t
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
        let paths = Paths::rooted(dir.path());
        let ctx = LaunchContext {
            cfg: &cfg,
            paths: &paths,
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
}
