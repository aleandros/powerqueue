//! Liveness and resource probes.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::domain::{ResourceSample, Session};
use crate::tmux::Tmux;

/// Result of checking on a session's tmux pane and process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProbe {
    /// The pane still exists in tmux.
    pub pane_exists: bool,
    /// The pane's command has exited (`remain-on-exit` keeps the pane).
    pub pane_dead: bool,
    pub exit_status: Option<i32>,
    /// The shell pid of the pane; children are Claude + tools.
    pub pane_pid: Option<u32>,
    /// The foreground command tmux reports (`node`, `claude`, `bash`...).
    pub current_command: Option<String>,
}

impl SessionProbe {
    pub fn is_alive(&self) -> bool {
        self.pane_exists && !self.pane_dead
    }
}

/// Inspect tmux for the session's pane.
pub fn probe_session(tmux: &Tmux, session: &Session) -> Result<SessionProbe> {
    let _ = (tmux, session);
    todo!("TODO(agent-runtime)")
}

/// CPU% and RSS summed over the pane's process tree.
pub fn sample_resources(system: &mut sysinfo::System, session: &Session) -> Option<ResourceSample> {
    let _ = (system, session);
    todo!("TODO(agent-runtime): refresh processes, walk parent links from pane_pid")
}
