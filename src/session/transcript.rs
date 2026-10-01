//! Incremental reader for Claude Code JSONL transcripts.
//!
//! Each assistant API response is written as one line *per content block*,
//! all sharing the same `message.id` and `message.usage`, so usage must be
//! counted once per message id. `input_tokens` is frequently a streaming
//! placeholder (0/1); cache fields and output tokens are reliable.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::domain::{TaskId, UsageRecord};

/// `~/.claude/projects/<encoded cwd>/<session id>.jsonl`.
pub fn transcript_path_for(claude_home: &Path, cwd: &Path, session_id: uuid::Uuid) -> PathBuf {
    let _ = (claude_home, cwd, session_id);
    todo!("TODO(agent-runtime): encode cwd by replacing every non-alphanumeric char with '-'")
}

/// `~/.claude` honouring `CLAUDE_CONFIG_DIR`.
pub fn claude_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude")
}

/// Tails one transcript file, remembering the byte offset and seen ids.
#[derive(Debug, Clone)]
pub struct TranscriptReader {
    pub path: PathBuf,
    pub offset: u64,
    pub seen: std::collections::HashSet<String>,
    pub session_id: uuid::Uuid,
    pub task_id: TaskId,
    /// Last assistant text seen (for idle heuristics / dashboard preview).
    pub last_text: Option<String>,
    /// Timestamp of the newest line read.
    pub last_line_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl TranscriptReader {
    pub fn new(path: PathBuf, session_id: uuid::Uuid, task_id: TaskId) -> Self {
        Self { path, offset: 0, seen: Default::default(), session_id, task_id, last_text: None, last_line_at: None }
    }

    /// Read lines appended since the last call and return *new* usage
    /// records (deduplicated by message id). Missing file = empty.
    pub fn read_new(&mut self) -> Result<Vec<UsageRecord>> {
        todo!("TODO(agent-runtime)")
    }

    /// Current file size, or 0 when missing (activity signal).
    pub fn file_len(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }
}

/// Parse one transcript line into a usage record, if it is an assistant
/// message with usage. Exposed for tests and `doctor`.
pub fn parse_line(line: &str, session_id: uuid::Uuid, task_id: TaskId) -> Option<UsageRecord> {
    let _ = (line, session_id, task_id);
    todo!("TODO(agent-runtime)")
}
