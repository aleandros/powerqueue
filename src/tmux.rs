//! tmux integration.
//!
//! One tmux *session* (default `powerqueue`) hosts one *window* per task.
//! Windows are named after the task key so `powerqueue attach ENG-123` is
//! simply `tmux select-window`. Panes keep `remain-on-exit` so a crashed
//! Claude process leaves its last screen for inspection.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Describes a pane as reported by `tmux list-panes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub window_id: String,
    pub window_name: String,
    pub pane_id: String,
    pub pane_pid: u32,
    pub dead: bool,
    /// Exit status of the pane's command once dead.
    pub dead_status: Option<i32>,
    pub current_command: String,
}

/// Returned by [`Tmux::new_window`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub window_id: String,
    pub pane_id: String,
    pub pane_pid: u32,
}

/// Wrapper around the `tmux` binary.
#[derive(Debug, Clone)]
pub struct Tmux {
    pub binary: String,
    pub socket_name: Option<String>,
}

impl Tmux {
    pub fn new(binary: impl Into<String>, socket_name: Option<String>) -> Self {
        Self { binary: binary.into(), socket_name }
    }

    /// Base command with `-L socket` applied.
    pub fn command(&self) -> std::process::Command {
        let mut c = std::process::Command::new(&self.binary);
        if let Some(sock) = &self.socket_name {
            c.arg("-L").arg(sock);
        }
        c
    }

    /// `tmux -V`, or an error if tmux is missing.
    pub fn version(&self) -> Result<String> {
        todo!("TODO(agent-runtime)")
    }

    pub fn has_session(&self, session: &str) -> Result<bool> {
        let _ = session;
        todo!("TODO(agent-runtime)")
    }

    /// Create the hosting session (detached) if needed. The first window is a
    /// plain shell named `powerqueue` so the session survives task windows closing.
    pub fn ensure_session(&self, session: &str, cwd: &Path) -> Result<()> {
        let _ = (session, cwd);
        todo!("TODO(agent-runtime)")
    }

    /// Create a window running `shell_command` (via `sh -c`) in `cwd`.
    /// Sets `remain-on-exit` when requested and returns ids + pane pid.
    pub fn new_window(&self, session: &str, name: &str, cwd: &Path, shell_command: &str, remain_on_exit: bool) -> Result<WindowInfo> {
        let _ = (session, name, cwd, shell_command, remain_on_exit);
        todo!("TODO(agent-runtime): tmux new-window -d -P -F with window_id pane_id pane_pid")
    }

    /// Re-run a command in an existing (dead) pane: `respawn-pane -k`.
    pub fn respawn_pane(&self, pane_id: &str, cwd: &Path, shell_command: &str) -> Result<WindowInfo> {
        let _ = (pane_id, cwd, shell_command);
        todo!("TODO(agent-runtime)")
    }

    pub fn list_panes(&self, session: &str) -> Result<Vec<PaneInfo>> {
        let _ = session;
        todo!("TODO(agent-runtime): list-panes -s -t session -F ... ; empty vec if session missing")
    }

    pub fn find_pane(&self, session: &str, pane_id: &str) -> Result<Option<PaneInfo>> {
        Ok(self.list_panes(session)?.into_iter().find(|p| p.pane_id == pane_id))
    }

    pub fn kill_window(&self, window_id: &str) -> Result<()> {
        let _ = window_id;
        todo!("TODO(agent-runtime)")
    }

    pub fn kill_session(&self, session: &str) -> Result<()> {
        let _ = session;
        todo!("TODO(agent-runtime)")
    }

    /// Send literal text followed by Enter to a pane.
    pub fn send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        let _ = (pane_id, text);
        todo!("TODO(agent-runtime): send-keys -l then Enter")
    }

    /// Send a key name such as `C-c` or `Escape`.
    pub fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        let _ = (pane_id, key);
        todo!("TODO(agent-runtime)")
    }

    /// Last `lines` lines of the pane's screen (including scrollback).
    pub fn capture_pane(&self, pane_id: &str, lines: u32) -> Result<String> {
        let _ = (pane_id, lines);
        todo!("TODO(agent-runtime): capture-pane -p -S -lines")
    }

    /// Argument vector to attach a terminal to `window` of `session`
    /// (switch-client when already inside tmux, attach-session otherwise).
    pub fn attach_args(&self, session: &str, window_id: Option<&str>) -> Vec<String> {
        let _ = (session, window_id);
        todo!("TODO(agent-runtime)")
    }

    /// Replace the current process with `tmux attach` (unix) or spawn and wait.
    pub fn attach(&self, session: &str, window_id: Option<&str>) -> Result<()> {
        let _ = (session, window_id);
        todo!("TODO(agent-runtime)")
    }

    /// True if the current process is running inside tmux (`$TMUX` set).
    pub fn inside_tmux() -> bool {
        std::env::var_os("TMUX").is_some()
    }
}

/// Quote a string for `sh -c`.
pub fn shell_quote(s: &str) -> String {
    shlex::try_quote(s).map(|c| c.into_owned()).unwrap_or_else(|_| format!("'{}'", s.replace('\'', "'\\''")))
}
