//! OpenAI Codex CLI (`codex`).
//!
//! Verified against codex-cli 0.159.2 (see
//! `docs/reference/providers-research.md`, appendix):
//!
//! * Launch: `codex -C <worktree> -m <model> <approval flags> -c notify=[...]
//!   [-c model_reasoning_effort=...] [-c projects=...] --add-dir ... "<prompt>"`.
//!   Codex has no `--session-id`; it generates a thread id.
//! * Completion: the `notify` program runs after every agent turn with a JSON
//!   payload as its last argument (`agent-turn-complete`); powerqueue points
//!   it at `powerqueue hook --provider codex ... --event Notify`. No trust
//!   prompt is involved and nothing is written to disk.
//! * Discovery: the rollout `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`
//!   whose first line (`session_meta`) has `cwd == <worktree>`.
//! * Usage: `event_msg/token_count` → `info.last_token_usage`; the model
//!   comes from the latest `turn_context`.
//! * Resume: `codex resume <thread id> ... "<prompt>"` (the rollout keeps
//!   growing in the same file).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};

use crate::budget::ObservedUsage;
use crate::config::{CODEX_APPROVAL_MODES, Config};
use crate::domain::{HookEvent, ModelTier, Provider, TaskId, TokenUsage, UsageRecord};
use crate::session::agent::{
    AgentCli, AgentLaunch, AuthStatus, LaunchContext, canonical, first_line, home_dir, matches_signature, prompt_arg, same_dir,
    session_env,
};
use crate::session::transcript::{TranscriptState, parse_timestamp};
use crate::store::Store;

/// The `--event` name powerqueue's `notify` command passes.
pub const NOTIFY_EVENT: &str = "Notify";

/// Codex runs a side turn on another thread to title the session; its
/// `notify` payload starts with this input message and must be ignored.
const TITLE_TURN_PREFIX: &str = "Generate a concise, single-line task title";

/// Lower-case rate-limit signatures (displayed errors, HTTP error type).
const SIGNATURES: [&str; 6] = [
    "hit your usage limit",
    "usage limit reached",
    "rate limit exceeded",
    "quota exceeded",
    "out of credits",
    "usage_limit_reached",
];

/// An assistant message longer than this is a real answer, never a bare
/// throttling error (avoids reading "fixed the rate limit exceeded bug" as one).
const MAX_ERROR_MESSAGE_CHARS: usize = 400;

/// How far before the session's start a rollout may be stamped (the tmux
/// window starts the CLI before the session row records its start time).
const DISCOVERY_SLACK: Duration = Duration::seconds(10);

/// OpenAI Codex CLI.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexCli;

/// `$CODEX_HOME` as the session sees it: `codex.env.CODEX_HOME`, else the
/// daemon's `CODEX_HOME`, else `~/.codex`.
pub fn codex_home(cfg: &Config) -> PathBuf {
    if let Some(dir) = cfg.codex.env.get("CODEX_HOME").filter(|d| !d.trim().is_empty()) {
        return crate::paths::expand_tilde(dir);
    }
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    home_dir().join(".codex")
}

/// Render `s` as a TOML basic string (quoted, with `\`, `"` and control
/// characters escaped), for `-c key=<value>` overrides whose value Codex
/// parses as TOML.
pub fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Flags for `codex.approval` (see [`CODEX_APPROVAL_MODES`]).
pub fn approval_flags(mode: &str) -> Result<Vec<String>> {
    let flags: &[&str] = match mode {
        "yolo" => &["--dangerously-bypass-approvals-and-sandbox"],
        "workspace-write" => &["-a", "never", "-s", "workspace-write"],
        "approve-for-me" => &["--approve-for-me"],
        "on-request" => &["-a", "on-request", "-s", "workspace-write"],
        other => bail!("codex.approval `{other}` is not one of {}", CODEX_APPROVAL_MODES.join("|")),
    };
    Ok(flags.iter().map(|s| s.to_string()).collect())
}

/// The `notify=[...]` override that routes turn-complete notifications to
/// `powerqueue hook --provider codex`.
pub fn notify_override(self_bin: &Path, task_id: TaskId, session_id: uuid::Uuid) -> String {
    let argv = [
        self_bin.to_string_lossy().to_string(),
        "hook".into(),
        "--provider".into(),
        "codex".into(),
        "--task".into(),
        task_id.to_string(),
        "--session".into(),
        session_id.to_string(),
        "--event".into(),
        NOTIFY_EVENT.into(),
    ];
    let items: Vec<String> = argv.iter().map(|a| toml_string(a)).collect();
    format!("notify=[{}]", items.join(","))
}

/// The override that marks `worktree` (as given and canonicalised) as a
/// trusted project so Codex skips its folder-trust dialog.
///
/// Written as one inline table on the `projects` key rather than the dotted
/// form `projects."<path>".trust_level`: Codex splits `-c` keys on `.`, and
/// worktree paths usually contain dots (`~/.local/share/...`).
pub fn trust_override(worktree: &Path) -> String {
    let mut keys = vec![worktree.to_string_lossy().to_string()];
    let physical = canonical(worktree).to_string_lossy().to_string();
    if !keys.contains(&physical) {
        keys.push(physical);
    }
    let entries: Vec<String> = keys.iter().map(|k| format!("{} = {{ trust_level = \"trusted\" }}", toml_string(k))).collect();
    format!("projects={{ {} }}", entries.join(", "))
}

/// Full `codex` command line for a launch (last element: the prompt).
pub fn codex_command(ctx: &LaunchContext<'_>) -> Result<Vec<String>> {
    let c = &ctx.cfg.codex;
    let wt = ctx.worktree.to_string_lossy().to_string();
    let mut argv = vec![c.binary.clone()];
    if let Some(thread) = ctx.resume {
        argv.extend(["resume".to_string(), thread.to_string()]);
    }
    argv.extend(["-C".to_string(), wt]);
    argv.extend(["-m".to_string(), ctx.model.alias().to_string()]);
    argv.extend(approval_flags(&c.approval)?);
    argv.extend(["-c".to_string(), notify_override(ctx.self_bin, ctx.task.id, ctx.session_id)]);
    if let Some(effort) = c.reasoning_effort.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        argv.extend(["-c".to_string(), format!("model_reasoning_effort={}", toml_string(effort))]);
    }
    if c.trust_workspace {
        argv.extend(["-c".to_string(), trust_override(ctx.worktree)]);
    }
    // The sandbox only lets the session write inside the worktree; the
    // completion command writes powerqueue's database and state, and git
    // writes the shared object store of the main checkout.
    let mut writable = vec![ctx.paths.data_dir.clone(), ctx.paths.state_dir.clone()];
    let git_dir = ctx.cfg.repo_path().join(".git");
    if git_dir.is_dir() {
        writable.push(git_dir);
    }
    for dir in writable {
        argv.extend(["--add-dir".to_string(), dir.to_string_lossy().to_string()]);
    }
    argv.extend(c.extra_args.iter().cloned());
    argv.push(prompt_arg(ctx.prompt_path));
    Ok(argv)
}

/// The `rate_limits` snapshot of a rollout `event_msg/token_count` line
/// (`{"limit_id":"codex","primary":{"used_percent":..,"window_minutes":..,
/// "resets_at":..},"secondary":..,..}`), if the line carries one.
pub fn rollout_rate_limits(line: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let payload = event_payload(&v, "token_count")?;
    payload.get("rate_limits").filter(|r| r.is_object()).cloned()
}

/// The message of a rollout `event_msg/error` line.
pub fn rollout_error_message(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let payload = event_payload(&v, "error")?;
    payload.get("message").and_then(|m| m.as_str()).map(str::to_string)
}

/// `payload` of an `event_msg` line whose `payload.type == kind`.
fn event_payload<'v>(v: &'v serde_json::Value, kind: &str) -> Option<&'v serde_json::Value> {
    if v.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    let payload = v.get("payload")?;
    (payload.get("type")?.as_str()? == kind).then_some(payload)
}

/// Usage from a `token_count` line (see the module docs).
pub fn parse_rollout_usage(line: &str, session_id: uuid::Uuid, task_id: TaskId, model: &ModelTier) -> Option<UsageRecord> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let payload = event_payload(&v, "token_count")?;
    let usage = payload.get("info")?.get("last_token_usage")?.as_object()?;
    let field = |name: &str| usage.get(name).and_then(|n| n.as_u64()).unwrap_or(0);
    let cached = field("cached_input_tokens");
    let timestamp_text = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or_default();
    let timestamp = parse_timestamp(timestamp_text).unwrap_or_else(Utc::now);
    // The thread id maps 1:1 to the powerqueue session id (a resume reuses
    // both). The timestamp guards against ordinals restarting on resume.
    let ordinal = v.get("ordinal").and_then(|o| o.as_u64()).map(|o| o.to_string()).unwrap_or_default();
    Some(UsageRecord {
        session_id,
        task_id,
        message_id: format!("codex-{session_id}-{ordinal}-{timestamp_text}"),
        tier: model.clone(),
        model_id: model.alias().to_string(),
        usage: TokenUsage {
            input_tokens: field("input_tokens").saturating_sub(cached),
            output_tokens: field("output_tokens"),
            cache_creation_input_tokens: field("cache_write_input_tokens"),
            cache_read_input_tokens: cached,
        },
        timestamp,
    })
}

/// Find the rollout of the session Codex started in `worktree` after
/// `started_after` under `<codex home>/sessions`. Returns `(thread id,
/// rollout path)` of the *earliest* session started since then: the main
/// thread is created first, so later rollouts in the same directory (a
/// following attempt, sub-agents) never shadow it.
pub fn find_rollout(sessions_dir: &Path, worktree: &Path, started_after: DateTime<Utc>) -> Result<Option<(String, PathBuf)>> {
    let not_before = started_after - DISCOVERY_SLACK;
    // Day directories use the local date: look one day further back.
    let first_day = (not_before - Duration::days(1)).date_naive();
    let mut best: Option<(DateTime<Utc>, String, PathBuf)> = None;
    for day in day_dirs(sessions_dir)? {
        if day.0 < first_day {
            continue;
        }
        let entries = match std::fs::read_dir(&day.1) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                continue;
            }
            let Some(modified) = entry.metadata().ok().and_then(|m| m.modified().ok()).map(DateTime::<Utc>::from) else {
                continue;
            };
            if modified < not_before {
                continue;
            }
            let Some((thread, cwd, started)) = read_session_meta(&path) else { continue };
            if started.is_some_and(|s| s < not_before) || !same_dir(Path::new(&cwd), worktree) {
                continue;
            }
            let born = started.unwrap_or(modified);
            if best.as_ref().is_none_or(|b| born < b.0) {
                best = Some((born, thread, path));
            }
        }
    }
    Ok(best.map(|(_, thread, path)| (thread, path)))
}

/// `(date, dir)` for every `YYYY/MM/DD` directory under `sessions_dir`.
fn day_dirs(sessions_dir: &Path) -> Result<Vec<(chrono::NaiveDate, PathBuf)>> {
    let mut out = Vec::new();
    let read = |p: &Path| -> Vec<(u32, PathBuf)> {
        std::fs::read_dir(p)
            .map(|rd| {
                rd.flatten().filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok().map(|n| (n, e.path()))).collect()
            })
            .unwrap_or_default()
    };
    if !sessions_dir.is_dir() {
        return Ok(out);
    }
    std::fs::read_dir(sessions_dir).with_context(|| format!("list {}", sessions_dir.display()))?;
    for (y, yp) in read(sessions_dir) {
        for (m, mp) in read(&yp) {
            for (d, dp) in read(&mp) {
                if let Some(date) = chrono::NaiveDate::from_ymd_opt(y as i32, m, d) {
                    out.push((date, dp));
                }
            }
        }
    }
    Ok(out)
}

/// `(thread id, cwd, session start)` from a rollout's first line.
fn read_session_meta(path: &Path) -> Option<(String, String, Option<DateTime<Utc>>)> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    let v: serde_json::Value = serde_json::from_str(first.trim()).ok()?;
    if v.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let p = v.get("payload")?;
    let thread = p.get("id").or_else(|| p.get("session_id"))?.as_str()?.to_string();
    let cwd = p.get("cwd")?.as_str()?.to_string();
    let started = p.get("timestamp").and_then(|t| t.as_str()).and_then(parse_timestamp);
    Some((thread, cwd, started))
}

/// True when an assistant message is a bare throttling error.
fn is_rate_limit_message(message: &str) -> bool {
    !message.contains(crate::domain::DONE_MARKER)
        && !message.contains(crate::domain::BLOCKED_MARKER)
        && message.chars().count() <= MAX_ERROR_MESSAGE_CHARS
        && matches_signature(message, &SIGNATURES)
}

impl AgentCli for CodexCli {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn prepare(&self, ctx: &LaunchContext<'_>) -> Result<AgentLaunch> {
        Ok(AgentLaunch {
            files: Vec::new(),
            env: session_env(&ctx.cfg.codex.env, ctx.task, ctx.session_id),
            argv: codex_command(ctx)?,
            transcript_path: None,
            poll_transcript_for_completion: false,
        })
    }

    /// Nothing to do: trust and `notify` travel on the command line.
    fn pre_launch(&self, _cfg: &Config, _repo: &Path, _worktree: &Path) -> Result<()> {
        Ok(())
    }

    fn discover_session(&self, cfg: &Config, worktree: &Path, started_after: DateTime<Utc>) -> Result<Option<(String, PathBuf)>> {
        find_rollout(&codex_home(cfg).join("sessions"), worktree, started_after)
    }

    fn parse_transcript_line(
        &self,
        line: &str,
        session_id: uuid::Uuid,
        task_id: TaskId,
        launched_model: &ModelTier,
    ) -> Option<UsageRecord> {
        parse_rollout_usage(line, session_id, task_id, launched_model)
    }

    /// Tracks `turn_context.model`, `session_meta` thread id, the latest
    /// `token_count.rate_limits`, `event_msg/error` throttling messages and
    /// `task_complete.last_agent_message`.
    fn observe_transcript_line(&self, line: &str, state: &mut TranscriptState) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("session_meta") => {
                if let Some(id) = v["payload"].get("id").and_then(|i| i.as_str()) {
                    state.agent_session_id = Some(id.to_string());
                }
            }
            Some("turn_context") => {
                if let Some(model) = v["payload"].get("model").and_then(|m| m.as_str()).filter(|m| !m.is_empty()) {
                    state.model = Some(ModelTier::from_model_id_for(Provider::Codex, model));
                }
            }
            Some("event_msg") => match v["payload"].get("type").and_then(|t| t.as_str()) {
                Some("token_count") => {
                    if let Some(r) = rollout_rate_limits(line) {
                        state.rate_limits = Some(r);
                    }
                }
                Some("error") => {
                    if let Some(message) = rollout_error_message(line)
                        && matches_signature(&message, &SIGNATURES)
                    {
                        state.rate_limit_errors.push(message);
                    }
                }
                Some("task_complete") => {
                    if let Some(m) = v["payload"].get("last_agent_message").and_then(|m| m.as_str()) {
                        state.pending_message = Some(m.to_string());
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// `Notify` with `agent-turn-complete` → `Stop` (title side turns are
    /// dropped); a throttling message or error → `StopFailure{rate_limit}`.
    fn normalize_hook(&self, event: &str, payload: serde_json::Value) -> Option<(HookEvent, serde_json::Value)> {
        if !event.eq_ignore_ascii_case(NOTIFY_EVENT) {
            return None;
        }
        let first_input = payload.get("input-messages").and_then(|m| m.get(0)).and_then(|m| m.as_str()).unwrap_or_default();
        if first_input.trim_start().starts_with(TITLE_TURN_PREFIX) {
            return None;
        }
        let message = payload.get("last-assistant-message").and_then(|m| m.as_str()).unwrap_or_default().to_string();
        let error = payload.get("error").and_then(|e| e.as_str().map(str::to_string).or_else(|| Some(e.to_string())));
        let throttled = error.as_deref().is_some_and(|e| matches_signature(e, &SIGNATURES)) || is_rate_limit_message(&message);
        if throttled {
            let text = error.filter(|e| matches_signature(e, &SIGNATURES)).unwrap_or_else(|| message.clone());
            return Some((
                HookEvent::StopFailure,
                serde_json::json!({
                    "error_type": "rate_limit",
                    "error": "rate_limit",
                    "error_message": text,
                    "last_assistant_message": message,
                    "session_id": payload.get("thread-id"),
                    "cwd": payload.get("cwd"),
                }),
            ));
        }
        if payload.get("type").and_then(|t| t.as_str()) != Some("agent-turn-complete") {
            return None;
        }
        Some((
            HookEvent::Stop,
            serde_json::json!({
                "last_assistant_message": message,
                "session_id": payload.get("thread-id"),
                "cwd": payload.get("cwd"),
            }),
        ))
    }

    fn rate_limit_signatures(&self) -> &'static [&'static str] {
        &SIGNATURES
    }

    /// `codex login status`: exit 0 means logged in; the first output line is the detail.
    fn auth_status(&self, binary: &str) -> Result<AuthStatus> {
        let out = std::process::Command::new(binary)
            .args(["login", "status"])
            .output()
            .with_context(|| format!("run `{binary} login status`"))?;
        Ok(AuthStatus { logged_in: out.status.success(), detail: first_line(&out.stdout, &out.stderr, "not logged in") })
    }

    fn allowed_modes(&self) -> &'static [&'static str] {
        &CODEX_APPROVAL_MODES
    }

    fn probe(&self, _cfg: &Config, _store: &Store) -> Result<Option<ObservedUsage>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Task, TaskSource};
    use crate::paths::Paths;
    use crate::session::agent::agent_for;

    const META: &str = r#"{"timestamp":"2026-10-02T03:24:50.216Z","ordinal":0,"type":"session_meta","payload":{"id":"01a0faa4-a7c0-73a0-b2ad-a6a92de2e116","session_id":"01a0faa4-a7c0-73a0-b2ad-a6a92de2e116","cwd":"CWD","originator":"codex-tui","cli_version":"0.159.2","source":"cli","timestamp":"2026-10-02T03:24:50.200Z"}}"#;
    const TURN: &str = r#"{"timestamp":"2026-10-02T03:24:51.634Z","ordinal":7,"type":"turn_context","payload":{"turn_id":"t1","cwd":"/w","approval_policy":"never","model":"gpt-6-luna"}}"#;
    const TOKENS: &str = r#"{"timestamp":"2026-10-02T03:24:53.780Z","ordinal":13,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":99999,"cached_input_tokens":1,"cache_write_input_tokens":0,"output_tokens":9,"reasoning_output_tokens":0,"total_tokens":1},"last_token_usage":{"input_tokens":18699,"cached_input_tokens":12032,"cache_write_input_tokens":7,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":18704},"model_context_window":258400},"rate_limits":{"limit_id":"codex","primary":{"used_percent":17.0,"window_minutes":10080,"resets_at":1791234430},"secondary":null,"plan_type":"prolite","rate_limit_reached_type":null}}}"#;
    const COMPLETE: &str = r#"{"timestamp":"2026-10-02T03:24:53.852Z","ordinal":14,"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"OK"}}"#;

    fn ctx_parts(dir: &Path) -> (Config, Task, Paths) {
        let mut cfg = Config::default();
        cfg.repo.path = dir.join("repo").display().to_string();
        let mut t = Task::new("ENG-7", "Do the thing", TaskSource::Manual);
        t.worktree_path = Some(dir.join("wt").display().to_string());
        (cfg, t, Paths::rooted(&dir.join("home")))
    }

    #[test]
    fn toml_strings_escape_quotes_backslashes_and_controls() {
        assert_eq!(toml_string("/plain/path"), r#""/plain/path""#);
        assert_eq!(toml_string(r#"/we"ird\path"#), r#""/we\"ird\\path""#);
        assert_eq!(toml_string("a\nb\tc\u{1}"), r#""a\nb\tc\u0001""#);
        // Round trip through a real TOML parser.
        for s in [r#"/a "b" \c"#, "/tmp/x.y/z", "é ünï\u{7f}", "it's"] {
            let doc: toml::Table = format!("k = {}", toml_string(s)).parse().unwrap();
            assert_eq!(doc["k"].as_str(), Some(s));
        }
    }

    #[test]
    fn overrides_parse_as_toml() {
        let notify = notify_override(Path::new(r#"/opt/p "q"/powerqueue"#), TaskId::new(), uuid::Uuid::nil());
        let (key, value) = notify.split_once('=').unwrap();
        assert_eq!(key, "notify");
        let doc: toml::Table = format!("v = {value}").parse().unwrap();
        let argv: Vec<&str> = doc["v"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(argv[0], r#"/opt/p "q"/powerqueue"#);
        assert_eq!(&argv[1..4], ["hook", "--provider", "codex"]);
        assert_eq!(argv.last(), Some(&"Notify"));

        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("we.ird \"wt\"");
        std::fs::create_dir_all(&wt).unwrap();
        let trust = trust_override(&wt);
        let (key, value) = trust.split_once('=').unwrap();
        assert_eq!(key, "projects");
        let doc: toml::Table = format!("v = {value}").parse().unwrap();
        let projects = doc["v"].as_table().unwrap();
        assert_eq!(projects[wt.to_string_lossy().as_ref()]["trust_level"].as_str(), Some("trusted"));
        assert_eq!(projects[canonical(&wt).to_string_lossy().as_ref()]["trust_level"].as_str(), Some("trusted"));
    }

    #[test]
    fn approval_map() {
        assert_eq!(approval_flags("yolo").unwrap(), ["--dangerously-bypass-approvals-and-sandbox"]);
        assert_eq!(approval_flags("workspace-write").unwrap(), ["-a", "never", "-s", "workspace-write"]);
        assert_eq!(approval_flags("approve-for-me").unwrap(), ["--approve-for-me"]);
        assert_eq!(approval_flags("on-request").unwrap(), ["-a", "on-request", "-s", "workspace-write"]);
        assert!(approval_flags("bogus").is_err());
        for mode in CODEX_APPROVAL_MODES {
            assert!(approval_flags(mode).is_ok(), "{mode}");
        }
    }

    #[test]
    fn prepare_builds_fresh_and_resume_argv() {
        let dir = tempfile::tempdir().unwrap();
        let (mut cfg, t, paths) = ctx_parts(dir.path());
        std::fs::create_dir_all(dir.path().join("repo/.git")).unwrap();
        cfg.codex.env.insert("OPENAI_FOO".into(), "1".into());
        cfg.codex.extra_args = vec!["--search".into()];
        let sid = uuid::Uuid::new_v4();
        let model = ModelTier::new("gpt-6.1-sol");
        let prompt = dir.path().join("prompt.md");
        let wt = PathBuf::from(t.worktree_path.clone().unwrap());
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
            self_bin: Path::new("/bin/powerqueue"),
        };
        let launch = agent_for(Provider::Codex).prepare(&ctx).unwrap();
        assert!(launch.files.is_empty());
        assert_eq!(launch.transcript_path, None);
        assert!(!launch.poll_transcript_for_completion);
        assert!(launch.env.iter().any(|(k, v)| k == "OPENAI_FOO" && v == "1"));
        assert!(launch.env.iter().any(|(k, v)| k == "POWERQUEUE_SESSION_ID" && *v == sid.to_string()));
        let a = &launch.argv;
        let wt_s = wt.to_string_lossy().to_string();
        assert_eq!(&a[..9], ["codex", "-C", wt_s.as_str(), "-m", "gpt-6.1-sol", "-a", "never", "-s", "workspace-write"]);
        assert_eq!(a[9], "-c");
        assert!(a[10].starts_with(r#"notify=["/bin/powerqueue","hook","--provider","codex","--task""#), "{}", a[10]);
        assert!(a[10].ends_with(&format!(r#""--session","{sid}","--event","Notify"]"#)), "{}", a[10]);
        assert_eq!(a[11..13], ["-c", r#"model_reasoning_effort="high""#]);
        assert_eq!(a[13], "-c");
        assert!(a[14].starts_with("projects={ "), "{}", a[14]);
        let data = paths.data_dir.to_string_lossy().to_string();
        let state = paths.state_dir.to_string_lossy().to_string();
        let git = dir.path().join("repo/.git").to_string_lossy().to_string();
        assert_eq!(a[15..21], ["--add-dir", data.as_str(), "--add-dir", state.as_str(), "--add-dir", git.as_str()]);
        assert_eq!(a[21], "--search");
        assert_eq!(a[22], prompt_arg(&prompt));
        assert_eq!(a.len(), 23);

        let mut cfg2 = cfg.clone();
        cfg2.codex.approval = "yolo".into();
        cfg2.codex.reasoning_effort = None;
        cfg2.codex.trust_workspace = false;
        cfg2.codex.extra_args.clear();
        let ctx = LaunchContext { cfg: &cfg2, resume: Some("01a0-thread"), attempt: 2, ..ctx };
        let a = agent_for(Provider::Codex).prepare(&ctx).unwrap().argv;
        assert_eq!(
            &a[..8],
            [
                "codex",
                "resume",
                "01a0-thread",
                "-C",
                wt_s.as_str(),
                "-m",
                "gpt-6.1-sol",
                "--dangerously-bypass-approvals-and-sandbox"
            ]
        );
        assert!(a[9].starts_with("notify=["));
        assert!(!a.iter().any(|x| x.starts_with("model_reasoning_effort") || x.starts_with("projects=")));
        assert_eq!(a.last().unwrap(), &prompt_arg(&prompt));

        let mut cfg3 = cfg2.clone();
        cfg3.codex.approval = "nope".into();
        let ctx = LaunchContext { cfg: &cfg3, ..ctx };
        assert!(agent_for(Provider::Codex).prepare(&ctx).is_err());
    }

    #[test]
    fn parses_token_count_with_cached_input_split_out() {
        let codex = agent_for(Provider::Codex);
        let sid = uuid::Uuid::new_v4();
        let tid = TaskId::new();
        let model = ModelTier::new("gpt-6-luna");
        let rec = codex.parse_transcript_line(TOKENS, sid, tid, &model).unwrap();
        assert_eq!(rec.usage.input_tokens, 18699 - 12032);
        assert_eq!(rec.usage.cache_read_input_tokens, 12032);
        assert_eq!(rec.usage.cache_creation_input_tokens, 7);
        assert_eq!(rec.usage.output_tokens, 5);
        assert_eq!(rec.tier, model);
        assert_eq!(rec.model_id, "gpt-6-luna");
        assert!(rec.message_id.starts_with(&format!("codex-{sid}-13-")), "{}", rec.message_id);
        assert_eq!(rec.timestamp.to_rfc3339(), "2026-10-02T03:24:53.780+00:00");
        for other in [META, TURN, COMPLETE, "not json", r#"{"type":"event_msg","payload":{"type":"token_count","info":null}}"#] {
            assert!(codex.parse_transcript_line(other, sid, tid, &model).is_none(), "{other}");
        }
    }

    #[test]
    fn observes_model_rate_limits_errors_and_final_message() {
        let codex = agent_for(Provider::Codex);
        let mut st = TranscriptState::default();
        for line in [META, TURN, TOKENS, COMPLETE] {
            codex.observe_transcript_line(line, &mut st);
        }
        assert_eq!(st.agent_session_id.as_deref(), Some("01a0faa4-a7c0-73a0-b2ad-a6a92de2e116"));
        assert_eq!(st.model, Some(ModelTier::new("gpt-6-luna")));
        assert_eq!(st.rate_limits.as_ref().unwrap()["primary"]["window_minutes"], 10080);
        assert_eq!(rollout_rate_limits(TOKENS).unwrap()["limit_id"], "codex");
        assert!(rollout_rate_limits(TURN).is_none());
        assert_eq!(st.take_pending_message().as_deref(), Some("OK"));
        assert!(st.take_rate_limit_errors().is_empty());

        let err = r#"{"timestamp":"2026-10-02T03:25:00Z","type":"event_msg","payload":{"type":"error","message":"You’ve hit your usage limit. Try again at 9:00 PM."}}"#;
        let other = r#"{"type":"event_msg","payload":{"type":"error","message":"stream disconnected"}}"#;
        codex.observe_transcript_line(err, &mut st);
        codex.observe_transcript_line(other, &mut st);
        assert_eq!(rollout_error_message(other).as_deref(), Some("stream disconnected"));
        assert_eq!(st.take_rate_limit_errors(), vec!["You’ve hit your usage limit. Try again at 9:00 PM.".to_string()]);
    }

    #[test]
    fn normalizes_notify_payloads() {
        let codex = agent_for(Provider::Codex);
        let turn = serde_json::json!({"type":"agent-turn-complete","thread-id":"01a0","turn-id":"t","cwd":"/w","client":"codex-tui","input-messages":["do it"],"last-assistant-message":"Done.\n[[POWERQUEUE:DONE]] shipped"});
        let (ev, p) = codex.normalize_hook("Notify", turn).unwrap();
        assert_eq!(ev, HookEvent::Stop);
        assert_eq!(p["last_assistant_message"], "Done.\n[[POWERQUEUE:DONE]] shipped");
        assert_eq!(p["session_id"], "01a0");
        assert_eq!(p["cwd"], "/w");

        let title = serde_json::json!({"type":"agent-turn-complete","thread-id":"other","input-messages":["Generate a concise, single-line task title for ..."],"last-assistant-message":"{\"title\":\"x\"}"});
        assert!(codex.normalize_hook("Notify", title).is_none());
        assert!(codex.normalize_hook("Stop", serde_json::json!({"type":"agent-turn-complete"})).is_none());
        assert!(codex.normalize_hook("Notify", serde_json::json!({"type":"something-else"})).is_none());

        let limited = serde_json::json!({"type":"agent-turn-complete","thread-id":"01a0","input-messages":["x"],"last-assistant-message":"You’ve hit your usage limit. Upgrade to Pro or try again later."});
        let (ev, p) = codex.normalize_hook("Notify", limited).unwrap();
        assert_eq!(ev, HookEvent::StopFailure);
        assert_eq!(p["error_type"], "rate_limit");
        assert_eq!(p["error"], "rate_limit");
        assert!(p["error_message"].as_str().unwrap().contains("usage limit"));

        let errored =
            serde_json::json!({"type":"agent-turn-complete","error":"usage_limit_reached","last-assistant-message":null});
        assert_eq!(codex.normalize_hook("notify", errored).unwrap().0, HookEvent::StopFailure);

        // A long answer that merely mentions a signature is a normal turn.
        let long = format!("I fixed the `rate limit exceeded` handling. {}", "Details. ".repeat(80));
        let (ev, _) = codex
            .normalize_hook("Notify", serde_json::json!({"type":"agent-turn-complete","last-assistant-message":long}))
            .unwrap();
        assert_eq!(ev, HookEvent::Stop);
        let marked = serde_json::json!({"type":"agent-turn-complete","last-assistant-message":"quota exceeded handled [[POWERQUEUE:DONE]]"});
        assert_eq!(codex.normalize_hook("Notify", marked).unwrap().0, HookEvent::Stop);
    }

    #[test]
    fn discovers_the_rollout_for_the_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        let wt = dir.path().join("wt");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let now = Utc::now();
        let day = home.join("sessions").join(now.format("%Y/%m/%d").to_string());
        std::fs::create_dir_all(&day).unwrap();
        let write = |name: &str, cwd: &Path, id: &str| {
            let meta = META
                .replace("CWD", &cwd.to_string_lossy())
                .replace("01a0faa4-a7c0-73a0-b2ad-a6a92de2e116", id)
                .replace("2026-10-02T03:24:50.200Z", &now.to_rfc3339());
            std::fs::write(day.join(name), format!("{meta}\n{TURN}\n")).unwrap();
        };
        let mut cfg = Config::default();
        cfg.codex.env.insert("CODEX_HOME".into(), home.display().to_string());
        let codex = agent_for(Provider::Codex);
        assert!(codex.discover_session(&cfg, &wt, now).unwrap().is_none());

        write("rollout-a-other.jsonl", &other, "thread-other");
        write("notes.txt", &wt, "ignored");
        std::fs::write(day.join("rollout-b-broken.jsonl"), "garbage\n").unwrap();
        assert!(codex.discover_session(&cfg, &wt, now).unwrap().is_none());

        // The canonical form of the worktree (e.g. /private/var vs /var) matches too.
        write("rollout-c-mine.jsonl", &canonical(&wt), "thread-mine");
        let (id, path) = codex.discover_session(&cfg, &wt, now).unwrap().unwrap();
        assert_eq!(id, "thread-mine");
        assert_eq!(path, day.join("rollout-c-mine.jsonl"));

        // A rollout started later in the same worktree (a sub-agent, the next attempt) does not shadow it.
        let later = META
            .replace("CWD", &wt.to_string_lossy())
            .replace("01a0faa4-a7c0-73a0-b2ad-a6a92de2e116", "thread-later")
            .replace("2026-10-02T03:24:50.200Z", &(now + Duration::seconds(30)).to_rfc3339());
        std::fs::write(day.join("rollout-d-later.jsonl"), format!("{later}\n")).unwrap();
        assert_eq!(codex.discover_session(&cfg, &wt, now).unwrap().unwrap().0, "thread-mine");

        // A session that started long before the launch is not ours.
        assert!(codex.discover_session(&cfg, &wt, now + Duration::hours(1)).unwrap().is_none());
        assert!(find_rollout(&dir.path().join("missing"), &wt, now).unwrap().is_none());
    }

    #[test]
    fn codex_home_prefers_session_env() {
        let mut cfg = Config::default();
        cfg.codex.env.insert("CODEX_HOME".into(), "/x/codex".into());
        assert_eq!(codex_home(&cfg), PathBuf::from("/x/codex"));
    }

    #[test]
    fn auth_status_from_scripts() {
        if which::which("sh").is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("codex-ok");
        std::fs::write(&ok, "#!/bin/sh\necho 'Logged in using ChatGPT'\n").unwrap();
        let no = dir.path().join("codex-no");
        std::fs::write(&no, "#!/bin/sh\necho 'Not logged in' >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in [&ok, &no] {
                std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let a = agent_for(Provider::Codex).auth_status(&ok.to_string_lossy()).unwrap();
        assert!(a.logged_in);
        assert_eq!(a.detail, "Logged in using ChatGPT");
        let a = agent_for(Provider::Codex).auth_status(&no.to_string_lossy()).unwrap();
        assert!(!a.logged_in);
        assert_eq!(a.detail, "Not logged in");
        assert!(agent_for(Provider::Codex).auth_status("/definitely/not/a/binary").is_err());
        let store = Store::open_in_memory().unwrap();
        assert!(agent_for(Provider::Codex).probe(&Config::default(), &store).unwrap().is_none());
    }
}
