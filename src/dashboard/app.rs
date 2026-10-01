//! Dashboard state (no terminal I/O; testable).

use chrono::{DateTime, Utc};

use crate::budget::Ledger;
use crate::domain::{Event, Session, Task};

/// Everything the UI needs for one frame.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub taken_at: Option<DateTime<Utc>>,
    pub daemon_alive: bool,
    pub daemon_pid: Option<u32>,
    pub tasks: Vec<Task>,
    pub sessions: Vec<Session>,
    pub ledger: Option<Ledger>,
    pub recent_events: Vec<Event>,
    pub max_concurrent: u32,
}

/// Selection, scroll and mode state.
#[derive(Debug, Clone, Default)]
pub struct DashboardApp {
    pub snapshot: Snapshot,
    pub selected: usize,
    pub show_help: bool,
    pub status_line: Option<String>,
    pub filter_terminal: bool,
}
