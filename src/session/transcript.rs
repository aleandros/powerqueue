//! Incremental reader for agent JSONL transcripts.
//!
//! The reader is provider-neutral: each complete line goes through the
//! provider's [`AgentCli::observe_transcript_line`](crate::session::agent::AgentCli::observe_transcript_line) (side state such as the
//! current model, rate-limit snapshots and errors, the last assistant
//! message) and [`AgentCli::parse_transcript_line`](crate::session::agent::AgentCli::parse_transcript_line) (usage records).
//!
//! Claude Code specifics: each assistant API response is written as one line *per content block*,
//! all sharing the same `message.id` and `message.usage`, so usage must be
//! counted once per message id. `input_tokens` is frequently a streaming
//! placeholder (0/1); cache fields and output tokens are reliable.

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::domain::{ModelTier, Provider, TaskId, TokenUsage, UsageRecord};
use crate::session::agent::agent_for;

/// Per-reader state a provider updates from transcript lines
/// ([`AgentCli::observe_transcript_line`](crate::session::agent::AgentCli::observe_transcript_line)). The trait is stateless; this is
/// where a provider keeps what later lines need (e.g. Codex reports the
/// model once per turn in `turn_context`, not on every usage line).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptState {
    /// Model the transcript last reported (Codex `turn_context.model`);
    /// used instead of the launched model when set.
    pub model: Option<ModelTier>,
    /// The provider's own session id, when the transcript names it (Codex
    /// `session_meta.payload.id`).
    pub agent_session_id: Option<String>,
    /// Latest rate-limit snapshot (Codex `token_count.rate_limits`).
    pub rate_limits: Option<serde_json::Value>,
    /// Rate-limit error messages not yet handed to the daemon.
    pub rate_limit_errors: Vec<String>,
    /// The newest assistant message not yet handed to the daemon (completion
    /// polling for CLIs without a reliable `Stop` hook).
    pub pending_message: Option<String>,
}

impl TranscriptState {
    /// Take the rate-limit errors seen since the last call.
    pub fn take_rate_limit_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.rate_limit_errors)
    }
    /// Take the newest assistant message seen since the last call.
    pub fn take_pending_message(&mut self) -> Option<String> {
        self.pending_message.take()
    }
}

/// `~/.claude/projects/<encoded cwd>/<session id>.jsonl`.
///
/// Claude Code encodes the working directory by replacing every character
/// that is not ASCII alphanumeric with `-` (`/Users/me/code/app` becomes
/// `-Users-me-code-app`).
pub fn transcript_path_for(claude_home: &Path, cwd: &Path, session_id: uuid::Uuid) -> PathBuf {
    claude_home.join("projects").join(encode_cwd(&cwd.to_string_lossy())).join(format!("{session_id}.jsonl"))
}

/// The project-directory encoding used by Claude Code.
pub fn encode_cwd(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
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
    /// Newest timestamped assistant response, excluding bookkeeping lines.
    pub last_progress_at: Option<DateTime<Utc>>,
    /// Which CLI wrote the transcript.
    pub provider: Provider,
    /// The model the session was launched with (fallback for usage lines
    /// that do not name their model).
    pub launched_model: ModelTier,
    /// Provider side state (see [`TranscriptState`]).
    pub state: TranscriptState,
}

impl TranscriptReader {
    /// A reader for a Claude Code transcript.
    pub fn new(path: PathBuf, session_id: uuid::Uuid, task_id: TaskId) -> Self {
        Self::for_provider(path, session_id, task_id, Provider::Claude, ModelTier::sonnet())
    }

    /// A reader for `provider`'s transcript of a session launched with `launched_model`.
    pub fn for_provider(
        path: PathBuf,
        session_id: uuid::Uuid,
        task_id: TaskId,
        provider: Provider,
        launched_model: ModelTier,
    ) -> Self {
        Self {
            path,
            offset: 0,
            seen: Default::default(),
            session_id,
            task_id,
            last_text: None,
            last_line_at: None,
            last_progress_at: None,
            provider,
            launched_model,
            state: TranscriptState::default(),
        }
    }

    /// Read lines appended since the last call and return *new* usage
    /// records (deduplicated by message id). Missing file = empty.
    ///
    /// Only complete lines (terminated by `\n`) are consumed; a partial
    /// trailing line is left for the next call. If the file shrank below the
    /// remembered offset (truncated or replaced) reading restarts from zero.
    pub fn read_new(&mut self) -> Result<Vec<UsageRecord>> {
        let file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("cannot open transcript {}", self.path.display())),
        };
        let len = file.metadata().with_context(|| format!("cannot stat transcript {}", self.path.display()))?.len();
        if len < self.offset {
            tracing::warn!(path = %self.path.display(), offset = self.offset, len, "transcript shrank; re-reading from start");
            self.offset = 0;
            self.seen.clear();
        }
        if len == self.offset {
            return Ok(Vec::new());
        }
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(self.offset)).with_context(|| format!("cannot seek transcript {}", self.path.display()))?;

        let mut records = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n =
                reader.read_until(b'\n', &mut buf).with_context(|| format!("cannot read transcript {}", self.path.display()))?;
            if n == 0 {
                break;
            }
            if buf.last() != Some(&b'\n') {
                // Partial line: Claude Code is still writing it.
                break;
            }
            self.offset += n as u64;
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.trim().is_empty() {
                continue;
            }
            self.observe_line(line);
            let agent = agent_for(self.provider);
            agent.observe_transcript_line(line, &mut self.state);
            if let Some(text) = &self.state.pending_message {
                self.last_text = Some(text.clone());
            }
            let model = self.state.model.as_ref().unwrap_or(&self.launched_model);
            if let Some(rec) = agent.parse_transcript_line(line, self.session_id, self.task_id, model)
                && self.seen.insert(rec.message_id.clone())
            {
                records.push(rec);
            }
        }
        if !records.is_empty() {
            tracing::debug!(task = %self.task_id, session = %self.session_id, new = records.len(), offset = self.offset, "read transcript usage");
        }
        Ok(records)
    }

    /// Update `last_line_at` / `last_text` from any complete line.
    fn observe_line(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
        if let Some(ts) = v.get("timestamp").or_else(|| v.get("created_at")).and_then(|t| t.as_str()).and_then(parse_timestamp) {
            self.last_line_at = Some(ts);
            if v.get("type").and_then(|t| t.as_str()) == Some("assistant")
                && v.get("isSidechain").and_then(|v| v.as_bool()) != Some(true)
            {
                self.last_progress_at = Some(self.last_progress_at.map_or(ts, |previous| previous.max(ts)));
            }
        }
        if v.get("type").and_then(|t| t.as_str()) == Some("assistant")
            && let Some(text) = newest_text_block(&v)
        {
            self.last_text = Some(text);
        }
    }

    /// Current file size, or 0 when missing (activity signal).
    pub fn file_len(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }
}

/// Parse one transcript line into a usage record, if it is an assistant
/// message with usage. Exposed for tests and `doctor`.
///
/// Only `type == "assistant"` lines whose `message` carries both an `id` and
/// a `usage` object qualify; everything else (user, system, summary, malformed
/// JSON) yields `None`. The timestamp comes from the line, or now if absent.
pub fn parse_line(line: &str, session_id: uuid::Uuid, task_id: TaskId) -> Option<UsageRecord> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let message = v.get("message")?;
    let message_id = message.get("id")?.as_str()?.to_string();
    let usage = message.get("usage")?.as_object()?;
    let field = |name: &str| usage.get(name).and_then(|n| n.as_u64()).unwrap_or(0);
    let model_id = message.get("model").and_then(|m| m.as_str()).unwrap_or_default().to_string();
    let timestamp = v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_timestamp).unwrap_or_else(Utc::now);
    Some(UsageRecord {
        session_id,
        task_id,
        message_id,
        tier: ModelTier::from_model_id(&model_id),
        model_id,
        usage: TokenUsage {
            input_tokens: field("input_tokens"),
            output_tokens: field("output_tokens"),
            cache_creation_input_tokens: field("cache_creation_input_tokens"),
            cache_read_input_tokens: field("cache_read_input_tokens"),
        },
        timestamp,
    })
}

pub(crate) fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc))
}

/// The last `text` content block of an assistant line, if any.
fn newest_text_block(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    content
        .as_array()?
        .iter()
        .rev()
        .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .and_then(|b| b.get("text").and_then(|t| t.as_str()))
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn assistant_line(msg_id: &str, block: &str, ts: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"u-{msg_id}","timestamp":"{ts}","message":{{"id":"{msg_id}","model":"claude-fable-5-1","role":"assistant","content":[{block}],"usage":{{"input_tokens":2,"output_tokens":100,"cache_creation_input_tokens":500,"cache_read_input_tokens":1000}}}}}}"#
        )
    }

    const TS: &str = "2026-10-01T23:15:26.413Z";

    fn ids() -> (uuid::Uuid, TaskId) {
        (uuid::Uuid::new_v4(), TaskId::new())
    }

    #[test]
    fn encodes_cwd_like_claude_code() {
        assert_eq!(encode_cwd("/Users/me/code/app"), "-Users-me-code-app");
        assert_eq!(encode_cwd("/tmp/a_b.c d"), "-tmp-a-b-c-d");
        let p = transcript_path_for(Path::new("/home/x/.claude"), Path::new("/Users/me/code/app"), uuid::Uuid::nil());
        assert_eq!(p, PathBuf::from("/home/x/.claude/projects/-Users-me-code-app/00000000-0000-0000-0000-000000000000.jsonl"));
    }

    #[test]
    fn parses_assistant_lines_only() {
        let (sid, tid) = ids();
        let rec = parse_line(&assistant_line("msg_1", r#"{"type":"text","text":"hi"}"#, TS), sid, tid).unwrap();
        assert_eq!(rec.message_id, "msg_1");
        assert_eq!(rec.model_id, "claude-fable-5-1");
        assert_eq!(rec.tier, ModelTier::fable());
        assert_eq!(rec.usage.output_tokens, 100);
        assert_eq!(rec.usage.cache_creation_input_tokens, 500);
        assert_eq!(rec.usage.cache_read_input_tokens, 1000);
        assert_eq!(rec.usage.input_tokens, 2);
        assert_eq!(rec.timestamp.to_rfc3339(), "2026-10-01T23:15:26.413+00:00");
        assert_eq!(rec.session_id, sid);
        assert_eq!(rec.task_id, tid);

        let user = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert!(parse_line(user, sid, tid).is_none());
        assert!(parse_line("{not json", sid, tid).is_none());
        assert!(parse_line("", sid, tid).is_none());
        // Assistant line without usage (e.g. a streamed placeholder) is ignored.
        let no_usage = r#"{"type":"assistant","message":{"id":"msg_2","content":[]}}"#;
        assert!(parse_line(no_usage, sid, tid).is_none());
        let no_id = r#"{"type":"assistant","message":{"usage":{"output_tokens":1}}}"#;
        assert!(parse_line(no_id, sid, tid).is_none());
    }

    #[test]
    fn missing_timestamp_defaults_to_now() {
        let (sid, tid) = ids();
        let line = r#"{"type":"assistant","message":{"id":"m","model":"claude-sonnet-5-5","usage":{"output_tokens":1}}}"#;
        let rec = parse_line(line, sid, tid).unwrap();
        assert!((Utc::now() - rec.timestamp).num_seconds() < 5);
        assert_eq!(rec.tier, ModelTier::sonnet());
    }

    #[test]
    fn dedupes_by_message_id_and_tracks_text() {
        let (sid, tid) = ids();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Three lines sharing msg_1 (thinking + text + tool_use), one user line, one malformed.
        writeln!(f, "{}", assistant_line("msg_1", r#"{"type":"thinking","thinking":"..."}"#, TS)).unwrap();
        writeln!(f, "{}", assistant_line("msg_1", r#"{"type":"text","text":"first answer"}"#, TS)).unwrap();
        writeln!(f, "{}", assistant_line("msg_1", r#"{"type":"tool_use","name":"Bash","input":{}}"#, TS)).unwrap();
        writeln!(f, r#"{{"type":"user","timestamp":"2026-10-01T23:16:00Z","message":{{"role":"user","content":"go on"}}}}"#)
            .unwrap();
        writeln!(f, "this is not json").unwrap();
        drop(f);

        let mut reader = TranscriptReader::new(path.clone(), sid, tid);
        let recs = reader.read_new().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].message_id, "msg_1");
        assert_eq!(reader.last_text.as_deref(), Some("first answer"));
        assert_eq!(reader.last_line_at.unwrap().to_rfc3339(), "2026-10-01T23:16:00+00:00");
        assert_eq!(reader.last_progress_at, parse_timestamp(TS), "user/bookkeeping lines are not assistant progress");
        assert_eq!(reader.offset, std::fs::metadata(&path).unwrap().len());

        // Nothing new: nothing returned.
        assert!(reader.read_new().unwrap().is_empty());

        // A repeated message id later in the file is still ignored.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", assistant_line("msg_1", r#"{"type":"text","text":"dup"}"#, TS)).unwrap();
        writeln!(f, "{}", assistant_line("msg_2", r#"{"type":"text","text":"second"}"#, TS)).unwrap();
        drop(f);
        let recs = reader.read_new().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].message_id, "msg_2");
        assert_eq!(reader.last_text.as_deref(), Some("second"));
    }

    #[test]
    fn usage_is_not_multiplied_by_content_blocks_or_reader_restarts() {
        let (sid, tid) = ids();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let line = assistant_line("msg_repeat", r#"{"type":"text","text":"answer"}"#, TS);
        std::fs::write(&path, format!("{line}\n{line}\n{line}\n")).unwrap();
        let store = crate::store::Store::open_in_memory().unwrap();
        for _ in 0..2 {
            let mut reader = TranscriptReader::new(path.clone(), sid, tid);
            for record in reader.read_new().unwrap() {
                store.record_usage(&record).unwrap();
            }
        }
        let usage = store.usage_for_task(tid).unwrap();
        assert_eq!(
            usage,
            TokenUsage { input_tokens: 2, output_tokens: 100, cache_creation_input_tokens: 500, cache_read_input_tokens: 1000 }
        );
        assert_eq!(usage.total(), 1602);
        assert_eq!(usage.weighted(), 1227.0);
    }

    #[test]
    fn partial_trailing_line_waits_for_completion() {
        let (sid, tid) = ids();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let full = assistant_line("msg_9", r#"{"type":"text","text":"done"}"#, TS);
        let (head, tail) = full.split_at(full.len() / 2);
        std::fs::write(&path, head).unwrap();

        let mut reader = TranscriptReader::new(path.clone(), sid, tid);
        assert!(reader.read_new().unwrap().is_empty());
        assert_eq!(reader.offset, 0);

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{tail}").unwrap();
        drop(f);
        let recs = reader.read_new().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].message_id, "msg_9");
        assert_eq!(reader.offset, full.len() as u64 + 1);
    }

    #[test]
    fn missing_file_and_truncation() {
        let (sid, tid) = ids();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.jsonl");
        let mut reader = TranscriptReader::new(path.clone(), sid, tid);
        assert!(reader.read_new().unwrap().is_empty());
        assert_eq!(reader.file_len(), 0);

        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", assistant_line("msg_a", r#"{"type":"text","text":"a"}"#, TS)).unwrap();
        writeln!(f, "{}", assistant_line("msg_b", r#"{"type":"text","text":"b"}"#, TS)).unwrap();
        drop(f);
        assert_eq!(reader.read_new().unwrap().len(), 2);

        // File replaced with something shorter: start over, old ids forgotten.
        std::fs::write(&path, format!("{}\n", assistant_line("msg_a", r#"{"type":"text","text":"a"}"#, TS))).unwrap();
        let recs = reader.read_new().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].message_id, "msg_a");
    }
}
