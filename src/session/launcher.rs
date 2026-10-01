//! Build everything a session needs on disk, then start it in tmux.
//!
//! Per task we write `<state>/tasks/<task-id>/`:
//! * `prompt.md`     – the task brief Claude receives as its first message
//! * `settings.json` – hooks that call back into `powerqueue hook`
//! * `launch.sh`     – the exact command line (kept for diagnostics; re-run to resume)
//! * `env`           – environment variables (0600)
//!
//! Resuming after a crash reuses the same Claude session id with `--resume`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::{ClaudeConfig, Config};
use crate::domain::{BLOCKED_MARKER, DONE_MARKER, ModelTier, Session, SessionState, Task, TaskId};
use crate::paths::Paths;
use crate::tmux::Tmux;

/// Files written for a launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPlan {
    pub task_dir: PathBuf,
    pub prompt_path: PathBuf,
    pub settings_path: PathBuf,
    pub script_path: PathBuf,
    /// The shell command tmux runs (`bash launch.sh`).
    pub shell_command: String,
    pub resume: bool,
}

/// Compose the first user message for a task.
pub fn build_prompt(task: &Task, cfg: &Config, attempt: u32, previous_error: Option<&str>) -> String {
    let _ = (task, cfg, attempt, previous_error);
    let _ = (DONE_MARKER, BLOCKED_MARKER);
    todo!("TODO(agent-runtime): title, description, source link, branch, rules: run `powerqueue task complete <id>` or print markers; attempt/previous error context")
}

/// Claude Code settings JSON wiring every relevant hook to `powerqueue hook`.
pub fn hook_settings(powerqueue_bin: &Path, task_id: TaskId, session_id: uuid::Uuid, claude: &ClaudeConfig) -> serde_json::Value {
    let _ = (powerqueue_bin, task_id, session_id, claude);
    todo!("TODO(agent-runtime): hooks map of event -> command '<bin> hook --task <id> --session <sid> --event <Event>'")
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
        let self_bin = std::env::current_exe()?;
        Ok(Self { paths, tmux, self_bin })
    }

    /// Write prompt, settings, env and launch script. `resume` = reuse the
    /// Claude session id (after a crash) instead of starting fresh.
    pub fn prepare(&self, cfg: &Config, task: &Task, session_id: uuid::Uuid, model: ModelTier, attempt: u32, resume: bool, previous_error: Option<&str>) -> Result<LaunchPlan> {
        let _ = (cfg, task, session_id, model, attempt, resume, previous_error);
        todo!("TODO(agent-runtime)")
    }

    /// Create the tmux window and return the new [`Session`] record.
    pub fn launch(&self, cfg: &Config, task: &Task, plan: &LaunchPlan, session_id: uuid::Uuid, model: ModelTier, attempt: u32) -> Result<Session> {
        let _ = (cfg, task, plan, session_id, model, attempt);
        let _ = (Utc::now(), SessionState::Launching);
        todo!("TODO(agent-runtime): ensure_session, new_window named task.slug(), build Session")
    }

    /// Full command line (for `task show` and debugging).
    pub fn claude_command(cfg: &Config, model: ModelTier, session_id: uuid::Uuid, settings_path: &Path, prompt_path: &Path, resume: bool, name: &str) -> Vec<String> {
        let _ = (cfg, model, session_id, settings_path, prompt_path, resume, name);
        todo!("TODO(agent-runtime): claude --session-id|--resume, --model, --permission-mode, --settings, --name, --effort, --fallback-model, extra args, prompt from file")
    }
}
