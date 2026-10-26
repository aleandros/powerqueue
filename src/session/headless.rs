//! Headless Claude Code sessions (`claude.session_mode = "headless"`):
//! `claude -p --output-format stream-json --verbose` as a background process
//! instead of the interactive CLI in a tmux window.
//!
//! Per attempt, `launch.sh` writes under `<task dir>/headless/`:
//! * `<session id>.<attempt>.stream.jsonl` – stdout, one JSON event per line
//!   (`system/init`, `assistant`, `user`, `system/api_retry`, ..., and a final
//!   `result` with the cost and session metadata)
//! * `<session id>.<attempt>.stderr.log` – stderr
//! * `<session id>.<attempt>.exit` – the CLI's exit status, written by
//!   `launch.sh` once the CLI exits
//!
//! `sh launch.sh` leads its own process group, so the session outlives a
//! daemon restart just like a tmux window does. The daemon records the
//! group's pid on the session, reads the stream for activity, usage, API
//! retries and the result ([`StreamReader`]), and decides how the attempt
//! ended from the exit status and the `result` message ([`HeadlessExit`],
//! [`crate::scheduler::transitions::on_headless_exit`]). Hooks, `powerqueue
//! task complete|block` and the Claude transcript work as in an interactive
//! session: `-p` loads the same `--settings`.
//!
//! Stopping a session (cancel, retry, pause, stale or timed out) sends
//! SIGTERM to the whole process group: Claude Code exits with 143, leaves
//! the current turn unfinished and runs its `SessionEnd` hooks; a resumed
//! attempt continues the conversation with a new prompt. A group still alive
//! [`KILL_GRACE`] later gets SIGKILL.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::{Session, TaskId, UsageRecord};
use crate::paths::Paths;

/// How long a stopped process group may take to exit after SIGTERM before
/// it gets SIGKILL.
pub const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// The flags that turn an interactive launch into a headless one; they go
/// right after the program in the argv.
pub const HEADLESS_FLAGS: [&str; 4] = ["-p", "--output-format", "stream-json", "--verbose"];

/// The files of one headless attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadlessFiles {
    /// stdout: the `stream-json` events.
    pub stream: PathBuf,
    /// stderr of the CLI.
    pub stderr: PathBuf,
    /// The exit status, written when the CLI exits.
    pub exit_status: PathBuf,
}

impl HeadlessFiles {
    /// The files of attempt `attempt` of session `session_id`, under `<task_dir>/headless/`.
    pub fn new(task_dir: &Path, session_id: uuid::Uuid, attempt: u32) -> Self {
        let dir = task_dir.join("headless");
        let base = format!("{session_id}.{attempt}");
        Self {
            stream: dir.join(format!("{base}.stream.jsonl")),
            stderr: dir.join(format!("{base}.stderr.log")),
            exit_status: dir.join(format!("{base}.exit")),
        }
    }

    /// The files of `session`'s current attempt.
    pub fn for_session(paths: &Paths, session: &Session) -> Self {
        Self::new(&paths.task_dir(&session.task_id.to_string()), session.id, session.attempt)
    }

    /// Create the directory and remove what an earlier launch of the same
    /// attempt left behind, so a stale exit status never ends a new run.
    pub fn reset(&self) -> Result<()> {
        if let Some(dir) = self.stream.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        for path in [&self.stream, &self.stderr, &self.exit_status] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("cannot remove {}", path.display())),
            }
        }
        Ok(())
    }

    /// The recorded exit status; `None` while the CLI runs, or when the
    /// wrapper was killed before it could write one.
    pub fn read_exit_status(&self) -> Option<i32> {
        std::fs::read_to_string(&self.exit_status).ok()?.trim().parse().ok()
    }

    /// The last `lines` non-empty lines of stderr, if there are any.
    pub fn stderr_tail(&self, lines: usize) -> Option<String> {
        let text = std::fs::read_to_string(&self.stderr).ok()?;
        let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = all[all.len().saturating_sub(lines)..].join("\n");
        (!tail.is_empty()).then_some(tail)
    }
}

/// The final `result` message of a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamResult {
    /// `success`, `error_max_turns`, `error_during_execution`, ...
    pub subtype: String,
    pub is_error: bool,
    /// The final response text (or the failure, for an error result).
    pub text: String,
    /// Claude Code's client-side cost estimate for the conversation.
    pub total_cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub duration_ms: Option<u64>,
    pub session_id: Option<String>,
    /// Tool calls denied because nobody could approve them.
    pub permission_denials: usize,
}

/// One line of the stream, reduced to what the daemon acts on.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// `system/init`: the session is up.
    Init { session_id: Option<String>, model: Option<String> },
    /// An `assistant` or `user` message: the agent is working. `subagent`
    /// when it belongs to a subagent run (`parent_tool_use_id` set).
    Message { subagent: bool },
    /// `system/api_retry`: an API call failed and is retried.
    ApiRetry { attempt: u64, max_retries: u64, error: String, error_status: Option<u64> },
    /// A tool call was denied because nobody could approve it.
    PermissionDenied { tool: Option<String>, message: String },
    /// The final `result` message.
    Result(StreamResult),
    /// Anything else (hook progress, partial messages, unknown types).
    Other,
}

impl StreamEvent {
    /// An `api_retry` for a usage or rate limit.
    pub fn is_rate_limit_retry(&self) -> bool {
        matches!(self, StreamEvent::ApiRetry { error, .. } if error == "rate_limit")
    }
}

/// Parse one stream line. `None` for blank lines and anything that is not a
/// JSON object.
pub fn parse_event(line: &str) -> Option<StreamEvent> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let ty = v.get("type")?.as_str()?;
    let str_of = |key: &str| v.get(key).and_then(|x| x.as_str()).map(str::to_string);
    let u64_of = |key: &str| v.get(key).and_then(serde_json::Value::as_u64);
    let subtype = str_of("subtype").unwrap_or_default();
    let event = match (ty, subtype.as_str()) {
        ("system", "init") => StreamEvent::Init { session_id: str_of("session_id"), model: str_of("model") },
        ("system", "api_retry") => StreamEvent::ApiRetry {
            attempt: u64_of("attempt").unwrap_or(0),
            max_retries: u64_of("max_retries").unwrap_or(0),
            error: str_of("error").unwrap_or_else(|| "unknown".to_string()),
            error_status: u64_of("error_status"),
        },
        ("system", "permission_denied") | ("permission_denied", _) => StreamEvent::PermissionDenied {
            tool: str_of("tool_name").or_else(|| str_of("tool")),
            message: str_of("message").or_else(|| str_of("reason")).unwrap_or_default(),
        },
        ("assistant" | "user", _) => StreamEvent::Message { subagent: v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) },
        ("result", _) => StreamEvent::Result(StreamResult {
            is_error: v.get("is_error").and_then(serde_json::Value::as_bool).unwrap_or(subtype != "success"),
            subtype,
            text: str_of("result").unwrap_or_default(),
            total_cost_usd: v.get("total_cost_usd").and_then(serde_json::Value::as_f64),
            num_turns: u64_of("num_turns"),
            duration_ms: u64_of("duration_ms"),
            session_id: str_of("session_id"),
            permission_denials: v.get("permission_denials").and_then(|d| d.as_array()).map_or(0, Vec::len),
        }),
        _ => StreamEvent::Other,
    };
    Some(event)
}

/// What one [`StreamReader::read_new`] call found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamBatch {
    /// Complete lines read.
    pub lines: usize,
    /// New usage records (one per assistant message id).
    pub usage: Vec<UsageRecord>,
    /// Events of lines written after [`StreamReader::signal_floor`]; older
    /// lines (a daemon restart replays the file) only update the reader's
    /// state.
    pub events: Vec<StreamEvent>,
}

/// Tails one stream file. Like the transcript reader it only consumes
/// complete lines; a truncated last line is never read.
#[derive(Debug, Clone)]
pub struct StreamReader {
    pub path: PathBuf,
    pub offset: u64,
    /// Lines that end at or before this offset were written before this
    /// reader's daemon started: their events are not reported again.
    pub signal_floor: u64,
    pub session_id: uuid::Uuid,
    pub task_id: TaskId,
    seen: HashSet<String>,
    /// The last `result` read.
    pub result: Option<StreamResult>,
    /// The newest API event was an `api_retry` for a rate limit (no message
    /// arrived since): a run that fails now failed on the limit.
    pub retrying_rate_limit: bool,
    /// The run already reported its current rate-limit burst.
    pub rate_limit_reported: bool,
}

impl StreamReader {
    /// A reader from the start of `path` that reports every event.
    pub fn new(path: PathBuf, session_id: uuid::Uuid, task_id: TaskId) -> Self {
        Self {
            path,
            offset: 0,
            signal_floor: 0,
            session_id,
            task_id,
            seen: HashSet::new(),
            result: None,
            retrying_rate_limit: false,
            rate_limit_reported: false,
        }
    }

    /// The same reader, but the lines already in the file were written
    /// before this daemon started (it restarted under a running session):
    /// they update the reader's state and usage without being reported again.
    #[must_use]
    pub fn replayed(mut self) -> Self {
        self.signal_floor = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        self
    }

    /// Read the lines appended since the last call. A missing file reads as
    /// empty; a file that shrank is read again from the start.
    #[expect(
        clippy::as_conversions,
        reason = "Collection and read lengths fit the platform counter type on supported 32-bit and 64-bit targets."
    )]
    pub fn read_new(&mut self) -> Result<StreamBatch> {
        let file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StreamBatch::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot open stream {}", self.path.display())),
        };
        let len = file.metadata().with_context(|| format!("cannot stat stream {}", self.path.display()))?.len();
        if len < self.offset {
            self.offset = 0;
            self.signal_floor = 0;
            self.seen.clear();
        }
        let mut batch = StreamBatch::default();
        if len == self.offset {
            return Ok(batch);
        }
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(self.offset)).with_context(|| format!("cannot seek stream {}", self.path.display()))?;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf).with_context(|| format!("cannot read stream {}", self.path.display()))?;
            if n == 0 || buf.last() != Some(&b'\n') {
                break;
            }
            self.offset += n as u64;
            batch.lines += 1;
            let line = String::from_utf8_lossy(&buf);
            let Some(event) = parse_event(&line) else { continue };
            if let Some(rec) = crate::session::transcript::parse_line(&line, self.session_id, self.task_id)
                && self.seen.insert(rec.message_id.clone())
            {
                batch.usage.push(rec);
            }
            match &event {
                StreamEvent::Message { .. } => {
                    self.retrying_rate_limit = false;
                    self.rate_limit_reported = false;
                }
                StreamEvent::ApiRetry { .. } => self.retrying_rate_limit = event.is_rate_limit_retry(),
                StreamEvent::Result(r) => self.result = Some(r.clone()),
                _ => {}
            }
            if self.offset > self.signal_floor && event != StreamEvent::Other {
                batch.events.push(event);
            }
        }
        Ok(batch)
    }
}

/// How a headless run ended, as far as the daemon can tell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HeadlessExit {
    /// The CLI's exit status; `None` when the wrapper died before writing it.
    pub exit_status: Option<i32>,
    /// The final `result` message; `None` for a missing or truncated stream.
    pub result: Option<StreamResult>,
    /// The run failed on a usage or rate limit (an error result naming one,
    /// or a failure right after `api_retry` events for `rate_limit`).
    pub rate_limited: bool,
    /// The last lines of stderr.
    pub stderr_tail: Option<String>,
}

impl HeadlessExit {
    /// Classify a finished run from its reader state and files.
    /// `rate_limit_signatures` are the provider's lower-case throttling phrases.
    pub fn observe(reader: &StreamReader, files: &HeadlessFiles, rate_limit_signatures: &[&str]) -> Self {
        let exit_status = files.read_exit_status();
        let result = reader.result.clone();
        let succeeded = exit_status == Some(0) && result.as_ref().is_some_and(|r| !r.is_error);
        let said_limit = result
            .as_ref()
            .is_some_and(|r| r.is_error && crate::session::agent::matches_signature(&r.text, rate_limit_signatures));
        let rate_limited = !succeeded && (said_limit || reader.retrying_rate_limit);
        Self { exit_status, result, rate_limited, stderr_tail: files.stderr_tail(20) }
    }

    /// The run produced a successful result and exited 0.
    pub fn succeeded(&self) -> bool {
        self.exit_status == Some(0) && self.result.as_ref().is_some_and(|r| !r.is_error)
    }

    /// Why the attempt did not finish, for `last_error` and the crash event.
    pub fn describe(&self) -> String {
        let status = match self.exit_status {
            Some(code) => format!("exited with status {code}"),
            None => "ended without an exit status (killed)".to_string(),
        };
        match &self.result {
            Some(r) if !r.is_error && self.exit_status == Some(0) => {
                "headless Claude Code ended its turn without a completion marker".to_string()
            }
            Some(r) if r.is_error => {
                let text = crate::scheduler::transitions::preview(&r.text, 200);
                if text.is_empty() {
                    format!("headless Claude Code run failed ({}) and {status}", r.subtype)
                } else {
                    format!("headless Claude Code run failed ({}): {text}; {status}", r.subtype)
                }
            }
            Some(_) => format!("headless Claude Code {status} after its result"),
            None => format!("headless Claude Code {status} without a result"),
        }
    }
}

/// Is a headless process still running?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessStatus {
    Running,
    Exited,
}

/// Check a headless session: [`HeadlessStatus::Exited`] once its exit status
/// is recorded, or when its process (group leader) is gone or is a different,
/// later process that reused the pid.
pub fn probe(system: &mut sysinfo::System, session: &Session, files: &HeadlessFiles) -> HeadlessStatus {
    if files.exit_status.exists() {
        return HeadlessStatus::Exited;
    }
    let Some(pid) = session.pid else { return HeadlessStatus::Exited };
    let pid = sysinfo::Pid::from_u32(pid);
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing().without_tasks(),
    );
    let Some(process) = system.process(pid) else { return HeadlessStatus::Exited };
    if process.status() == sysinfo::ProcessStatus::Zombie {
        return HeadlessStatus::Exited;
    }
    // A process that started well after the session did is not ours.
    let started = i64::try_from(process.start_time()).unwrap_or(i64::MAX);
    if started > session.started_at.timestamp() + 120 {
        return HeadlessStatus::Exited;
    }
    HeadlessStatus::Running
}

/// Stop `session`'s process group if it is a headless session that is still
/// running (checked first, so a long-finished session's reused pid is never
/// signalled). Returns whether a signal was sent; `Ok(false)` for tmux
/// sessions.
pub fn stop_session(paths: &Paths, session: &Session) -> Result<bool> {
    if !session.is_headless() {
        return Ok(false);
    }
    let files = HeadlessFiles::for_session(paths, session);
    let Some(pid) = session.pid else { return Ok(false) };
    if probe(&mut sysinfo::System::new(), session, &files) == HeadlessStatus::Exited {
        return Ok(false);
    }
    stop(pid)?;
    tracing::info!(task = %session.task_id, session = %session.id, pid, "stopped headless session (SIGTERM to its process group)");
    Ok(true)
}

/// Start `sh <script>` in `worktree` as the leader of a new process group,
/// with no terminal: stdin from `/dev/null`, its own output discarded (the
/// script redirects the CLI's). A thread reaps it, so an exited session never
/// lingers as a zombie while this process runs. Returns the pid.
pub fn spawn(script: &Path, worktree: &Path) -> Result<u32> {
    let mut command = std::process::Command::new("sh");
    command
        .arg(script)
        .current_dir(worktree)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child =
        command.spawn().with_context(|| format!("cannot start `sh {}` in {}", script.display(), worktree.display()))?;
    let pid = child.id();
    std::thread::Builder::new()
        .name(format!("headless-{pid}"))
        .spawn(move || {
            let _ = child.wait();
        })
        .context("cannot start the headless session reaper")?;
    Ok(pid)
}

/// Stop a headless session: SIGTERM to its process group now, SIGKILL to
/// whatever is left after [`KILL_GRACE`] (from a background thread). A
/// group that is already gone is not an error.
pub fn stop(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process_group};
        let group = i32::try_from(pid).ok().and_then(Pid::from_raw).context("invalid process group id")?;
        match kill_process_group(group, Signal::TERM) {
            Ok(()) => {}
            Err(rustix::io::Errno::SRCH) => return Ok(()),
            Err(e) => return Err(std::io::Error::from(e)).with_context(|| format!("cannot signal process group {pid}")),
        }
        std::thread::Builder::new()
            .name(format!("headless-kill-{pid}"))
            .spawn(move || {
                std::thread::sleep(KILL_GRACE);
                let _ = kill_process_group(group, Signal::KILL);
            })
            .context("cannot start the headless kill timer")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let mut system = sysinfo::System::new();
        let pid = sysinfo::Pid::from_u32(pid);
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        if let Some(p) = system.process(pid) {
            p.kill();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded `stream-json` lines (Claude Code 2.1, trimmed to the fields
    /// powerqueue reads plus a few it must ignore).
    const INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/wt/eng-1","session_id":"6f2c1d1e-0000-4000-8000-000000000001","tools":["Bash","Read"],"mcp_servers":[],"model":"claude-opus-5-5","permissionMode":"acceptEdits","apiKeySource":"none","uuid":"u0"}"#;
    const ASSISTANT_TEXT: &str = r#"{"type":"assistant","message":{"id":"msg_01","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{"type":"text","text":"Looking at the tests."}],"stop_reason":null,"usage":{"input_tokens":3,"cache_creation_input_tokens":2400,"cache_read_input_tokens":11000,"output_tokens":120}},"parent_tool_use_id":null,"session_id":"6f2c1d1e-0000-4000-8000-000000000001","uuid":"u1"}"#;
    const ASSISTANT_TOOL: &str = r#"{"type":"assistant","message":{"id":"msg_01","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}],"stop_reason":null,"usage":{"input_tokens":3,"cache_creation_input_tokens":2400,"cache_read_input_tokens":11000,"output_tokens":120}},"parent_tool_use_id":null,"session_id":"6f2c1d1e-0000-4000-8000-000000000001","uuid":"u2"}"#;
    const USER_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"ok"}]},"parent_tool_use_id":null,"session_id":"6f2c1d1e-0000-4000-8000-000000000001","uuid":"u3"}"#;
    const ASSISTANT_DONE: &str = r#"{"type":"assistant","message":{"id":"msg_02","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{"type":"text","text":"All tests pass.\n[[POWERQUEUE:DONE]] fixed the flaky test"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"cache_creation_input_tokens":300,"cache_read_input_tokens":13400,"output_tokens":45}},"parent_tool_use_id":null,"session_id":"6f2c1d1e-0000-4000-8000-000000000001","uuid":"u4"}"#;
    const RESULT_OK: &str = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":81234,"duration_api_ms":60100,"num_turns":4,"result":"All tests pass.\n[[POWERQUEUE:DONE]] fixed the flaky test","session_id":"6f2c1d1e-0000-4000-8000-000000000001","total_cost_usd":0.4213,"usage":{"input_tokens":4,"output_tokens":165},"permission_denials":[],"uuid":"u5"}"#;
    const RETRY_LIMIT: &str = r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"retry_delay_ms":30000,"error_status":429,"error":"rate_limit","uuid":"r1","session_id":"6f2c1d1e-0000-4000-8000-000000000001"}"#;
    const RETRY_SERVER: &str = r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"retry_delay_ms":1000,"error_status":529,"error":"overloaded","uuid":"r2","session_id":"6f2c1d1e-0000-4000-8000-000000000001"}"#;
    const RESULT_LIMIT: &str = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":312000,"num_turns":2,"result":"Claude AI usage limit reached|1793000000","session_id":"6f2c1d1e-0000-4000-8000-000000000001","total_cost_usd":0.05,"permission_denials":[]}"#;
    const RESULT_FAIL: &str = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"duration_ms":4000,"num_turns":30,"result":"","session_id":"6f2c1d1e-0000-4000-8000-000000000001","permission_denials":[{"tool_name":"Bash","tool_use_id":"toolu_9","tool_input":{"command":"rm -rf /"}}]}"#;
    const DENIED: &str = r#"{"type":"system","subtype":"permission_denied","tool_name":"Bash","message":"Nobody can approve this request","uuid":"d1"}"#;

    const SIGS: [&str; 4] = ["usage limit reached", "rate_limit", "rate limit", "overloaded"];

    fn write(dir: &Path, lines: &[&str], trailing: &str) -> PathBuf {
        let path = dir.join("stream.jsonl");
        let mut text = lines.iter().fold(String::new(), |mut acc, l| {
            acc.push_str(l);
            acc.push('\n');
            acc
        });
        text.push_str(trailing);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn files(dir: &Path, exit: Option<i32>, stderr: &str) -> HeadlessFiles {
        let files = HeadlessFiles::new(dir, uuid::Uuid::nil(), 1);
        std::fs::create_dir_all(files.stream.parent().unwrap()).unwrap();
        if let Some(code) = exit {
            std::fs::write(&files.exit_status, format!("{code}\n")).unwrap();
        }
        std::fs::write(&files.stderr, stderr).unwrap();
        files
    }

    fn read_all(path: PathBuf) -> (StreamReader, StreamBatch) {
        let mut reader = StreamReader::new(path, uuid::Uuid::new_v4(), TaskId::new());
        let batch = reader.read_new().unwrap();
        (reader, batch)
    }

    #[test]
    fn parses_the_event_kinds() {
        assert_eq!(
            parse_event(INIT),
            Some(StreamEvent::Init {
                session_id: Some("6f2c1d1e-0000-4000-8000-000000000001".into()),
                model: Some("claude-opus-5-5".into())
            })
        );
        assert_eq!(parse_event(ASSISTANT_TEXT), Some(StreamEvent::Message { subagent: false }));
        assert_eq!(parse_event(USER_RESULT), Some(StreamEvent::Message { subagent: false }));
        let sub = ASSISTANT_TEXT.replace(r#""parent_tool_use_id":null"#, r#""parent_tool_use_id":"toolu_7""#);
        assert_eq!(parse_event(&sub), Some(StreamEvent::Message { subagent: true }));
        let retry = parse_event(RETRY_LIMIT).unwrap();
        assert_eq!(
            retry,
            StreamEvent::ApiRetry { attempt: 1, max_retries: 10, error: "rate_limit".into(), error_status: Some(429) }
        );
        assert!(retry.is_rate_limit_retry());
        assert!(!parse_event(RETRY_SERVER).unwrap().is_rate_limit_retry());
        assert_eq!(
            parse_event(DENIED),
            Some(StreamEvent::PermissionDenied { tool: Some("Bash".into()), message: "Nobody can approve this request".into() })
        );
        let Some(StreamEvent::Result(r)) = parse_event(RESULT_OK) else { panic!("result") };
        assert_eq!(r.subtype, "success");
        assert!(!r.is_error);
        assert_eq!(r.total_cost_usd, Some(0.4213));
        assert_eq!(r.num_turns, Some(4));
        assert_eq!(r.duration_ms, Some(81234));
        assert!(r.text.contains("[[POWERQUEUE:DONE]]"));
        let Some(StreamEvent::Result(r)) = parse_event(RESULT_FAIL) else { panic!("result") };
        assert!(r.is_error);
        assert_eq!(r.permission_denials, 1);
        assert_eq!(parse_event(r#"{"type":"stream_event","event":{}}"#), Some(StreamEvent::Other));
        assert_eq!(parse_event("not json"), None);
        assert_eq!(parse_event(""), None);
        assert_eq!(parse_event("[1,2]"), None);
    }

    #[test]
    fn success_stream_counts_each_message_once_and_classifies_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &[INIT, ASSISTANT_TEXT, ASSISTANT_TOOL, USER_RESULT, ASSISTANT_DONE, RESULT_OK], "");
        let (reader, batch) = read_all(path);
        assert_eq!(batch.lines, 6);
        let ids: Vec<&str> = batch.usage.iter().map(|u| u.message_id.as_str()).collect();
        assert_eq!(ids, ["msg_01", "msg_02"], "content blocks of one message share its usage");
        assert_eq!(batch.usage[0].usage.output_tokens, 120);
        assert_eq!(batch.usage[0].usage.cache_read_input_tokens, 11000);
        assert_eq!(batch.usage[1].tier, crate::domain::ModelTier::opus());
        assert_eq!(batch.events.len(), 6);
        let exit = HeadlessExit::observe(&reader, &files(dir.path(), Some(0), ""), &SIGS);
        assert!(exit.succeeded());
        assert!(!exit.rate_limited);
        assert_eq!(exit.result.as_ref().and_then(|r| r.total_cost_usd), Some(0.4213));
        assert_eq!(exit.describe(), "headless Claude Code ended its turn without a completion marker");
    }

    #[test]
    fn usage_matches_what_the_transcript_reader_records() {
        // The transcript writes the same assistant lines (plus a timestamp):
        // both readers must yield identical usage per message id.
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &[ASSISTANT_TEXT, ASSISTANT_TOOL, ASSISTANT_DONE], "");
        let (_, batch) = read_all(path);
        let transcript = dir.path().join("t.jsonl");
        let lines = [ASSISTANT_TEXT, ASSISTANT_TOOL, ASSISTANT_DONE].iter().fold(String::new(), |mut acc, l| {
            acc.push_str(&l.replacen('{', r#"{"timestamp":"2026-10-01T12:00:00Z","#, 1));
            acc.push('\n');
            acc
        });
        std::fs::write(&transcript, lines).unwrap();
        let mut t = crate::session::TranscriptReader::new(transcript, uuid::Uuid::new_v4(), TaskId::new());
        let records = t.read_new().unwrap();
        let usage = |r: &[UsageRecord]| r.iter().map(|u| (u.message_id.clone(), u.usage, u.tier.clone())).collect::<Vec<_>>();
        assert_eq!(usage(&batch.usage), usage(&records));
    }

    #[test]
    fn failure_stream_is_a_crash_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &[INIT, ASSISTANT_TEXT, RESULT_FAIL], "");
        let (reader, _) = read_all(path);
        let exit = HeadlessExit::observe(&reader, &files(dir.path(), Some(1), "boom\n\nError: max turns\n"), &SIGS);
        assert!(!exit.succeeded());
        assert!(!exit.rate_limited);
        assert_eq!(exit.stderr_tail.as_deref(), Some("boom\nError: max turns"));
        assert_eq!(exit.describe(), "headless Claude Code run failed (error_max_turns) and exited with status 1");
    }

    #[test]
    fn rate_limit_streams_are_throttling() {
        let dir = tempfile::tempdir().unwrap();
        // Retries for a rate limit, then the run gives up with a limit message.
        let path = write(dir.path(), &[INIT, ASSISTANT_TEXT, RETRY_LIMIT, RETRY_LIMIT, RESULT_LIMIT], "");
        let (reader, batch) = read_all(path);
        assert_eq!(batch.events.iter().filter(|e| e.is_rate_limit_retry()).count(), 2);
        let exit = HeadlessExit::observe(&reader, &files(dir.path(), Some(1), ""), &SIGS);
        assert!(exit.rate_limited, "{exit:?}");
        assert!(exit.describe().contains("usage limit reached"), "{}", exit.describe());

        // Killed while retrying a rate limit: no result, still throttling.
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &[INIT, RETRY_LIMIT], "");
        let (reader, _) = read_all(path);
        assert!(HeadlessExit::observe(&reader, &files(dir.path(), None, ""), &SIGS).rate_limited);

        // A retried rate limit that recovered does not throttle a later failure.
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &[INIT, RETRY_LIMIT, ASSISTANT_TEXT, RESULT_FAIL], "");
        let (reader, _) = read_all(path);
        assert!(!HeadlessExit::observe(&reader, &files(dir.path(), Some(1), ""), &SIGS).rate_limited);

        // Overloaded retries are not a usage limit.
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &[INIT, RETRY_SERVER], "");
        let (reader, _) = read_all(path);
        assert!(!HeadlessExit::observe(&reader, &files(dir.path(), Some(1), ""), &SIGS).rate_limited);
    }

    #[test]
    fn truncated_stream_has_no_result_and_is_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        // Killed mid-write: the last line has no newline (and is cut short).
        let cut = &RESULT_OK[..RESULT_OK.len() / 2];
        let path = write(dir.path(), &[INIT, ASSISTANT_TEXT], cut);
        let (mut reader, batch) = read_all(path.clone());
        assert_eq!(batch.lines, 2, "the partial line is not consumed");
        assert!(reader.result.is_none());
        let exit = HeadlessExit::observe(&reader, &files(dir.path(), Some(143), ""), &SIGS);
        assert!(!exit.succeeded());
        assert_eq!(exit.describe(), "headless Claude Code exited with status 143 without a result");
        let killed = files(dir.path(), None, "");
        std::fs::remove_file(&killed.exit_status).unwrap();
        let exit = HeadlessExit::observe(&reader, &killed, &SIGS);
        assert_eq!(exit.describe(), "headless Claude Code ended without an exit status (killed) without a result");

        // The rest of the line arrives later: it is read then, once.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&RESULT_OK[RESULT_OK.len() / 2..]);
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        let batch = reader.read_new().unwrap();
        assert_eq!(batch.lines, 1);
        assert!(reader.result.is_some());
        assert_eq!(reader.read_new().unwrap().lines, 0);
    }

    #[test]
    fn replayed_lines_update_state_without_reporting_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &[INIT, RETRY_LIMIT], "");
        let mut reader = StreamReader::new(path.clone(), uuid::Uuid::new_v4(), TaskId::new()).replayed();
        let batch = reader.read_new().unwrap();
        assert_eq!(batch.lines, 2);
        assert!(batch.events.is_empty(), "written before this daemon started");
        assert!(reader.retrying_rate_limit);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(ASSISTANT_TEXT);
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        let batch = reader.read_new().unwrap();
        assert_eq!(batch.events, vec![StreamEvent::Message { subagent: false }]);
        assert_eq!(batch.usage.len(), 1);
        assert!(!reader.retrying_rate_limit);
        // A missing file is empty, not an error.
        let mut missing = StreamReader::new(dir.path().join("nope"), uuid::Uuid::new_v4(), TaskId::new()).replayed();
        assert_eq!(missing.read_new().unwrap(), StreamBatch::default());
    }

    #[test]
    fn files_reset_and_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let sid = uuid::Uuid::new_v4();
        let files = HeadlessFiles::new(dir.path(), sid, 3);
        assert_eq!(files.stream, dir.path().join(format!("headless/{sid}.3.stream.jsonl")));
        assert_eq!(files.stderr, dir.path().join(format!("headless/{sid}.3.stderr.log")));
        assert_eq!(files.exit_status, dir.path().join(format!("headless/{sid}.3.exit")));
        files.reset().unwrap();
        assert_eq!(files.read_exit_status(), None);
        assert_eq!(files.stderr_tail(5), None);
        std::fs::write(&files.exit_status, "137\n").unwrap();
        assert_eq!(files.read_exit_status(), Some(137));
        files.reset().unwrap();
        assert!(!files.exit_status.exists(), "a new launch never sees the old status");
    }

    #[cfg(unix)]
    #[test]
    fn spawn_probe_and_stop_a_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let files = HeadlessFiles::new(dir.path(), uuid::Uuid::new_v4(), 1);
        files.reset().unwrap();
        let script = dir.path().join("launch.sh");
        // The child shell ignores nothing: SIGTERM to the group ends both.
        std::fs::write(
            &script,
            format!(
                "sleep 30 >{out} 2>{err}\nstatus=$?\nprintf '%s\\n' \"$status\" >{exit}\nexit \"$status\"\n",
                out = files.stream.display(),
                err = files.stderr.display(),
                exit = files.exit_status.display()
            ),
        )
        .unwrap();
        let started = chrono::Utc::now() - chrono::Duration::seconds(1);
        let pid = spawn(&script, dir.path()).unwrap();
        let mut session = Session {
            id: uuid::Uuid::new_v4(),
            task_id: TaskId::new(),
            attempt: 1,
            model: crate::domain::ModelTier::sonnet(),
            state: crate::domain::SessionState::Running,
            tmux_session: String::new(),
            tmux_window: String::new(),
            pane_id: None,
            pid: Some(pid),
            transcript_path: None,
            exit_code: None,
            started_at: started,
            ended_at: None,
            last_activity_at: started,
            error: None,
            agent_session_id: None,
            waiting_since: None,
            waited_secs: 0,
            runtime: crate::domain::SessionRuntime::Headless,
        };
        let mut system = sysinfo::System::new();
        assert_eq!(probe(&mut system, &session, &files), HeadlessStatus::Running);
        stop(pid).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while probe(&mut system, &session, &files) == HeadlessStatus::Running {
            assert!(std::time::Instant::now() < deadline, "process group still running after SIGTERM");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Stopping a group that is gone is fine.
        std::thread::sleep(std::time::Duration::from_millis(200));
        stop(pid).unwrap();

        // A normal exit records its status.
        std::fs::write(
            &script,
            format!(
                "sh -c 'exit 3' >{out} 2>{err}\nstatus=$?\nprintf '%s\\n' \"$status\" >{exit}\nexit \"$status\"\n",
                out = files.stream.display(),
                err = files.stderr.display(),
                exit = files.exit_status.display()
            ),
        )
        .unwrap();
        files.reset().unwrap();
        let pid = spawn(&script, dir.path()).unwrap();
        session.pid = Some(pid);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while probe(&mut system, &session, &files) == HeadlessStatus::Running {
            assert!(std::time::Instant::now() < deadline, "process did not exit");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while files.read_exit_status().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(files.read_exit_status(), Some(3));

        // No pid, or a pid that is not running: exited.
        session.pid = None;
        files.reset().unwrap();
        assert_eq!(probe(&mut system, &session, &files), HeadlessStatus::Exited);
        session.pid = Some(u32::MAX - 1);
        assert_eq!(probe(&mut system, &session, &files), HeadlessStatus::Exited);
    }
}
