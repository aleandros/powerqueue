//! The session inbox: how `powerqueue task complete|block` and the provider
//! hooks reach the daemon when a session runs where the daemon's own
//! `powerqueue` binary cannot (inside a container, on another OS).
//!
//! With `<provider>.shim = true` the launcher writes a POSIX shell script to
//! `<task dir>/bin/powerqueue` ([`shim_script`]) and points the hooks, the
//! Codex `notify` command and the prompt's completion protocol at it. The
//! script knows three commands and turns each into one file in
//! `<task dir>/inbox/` (written to a temporary name and renamed, so the
//! daemon never sees a partial file):
//!
//! * `hook [--provider P] --task T [--session S] --event E [PAYLOAD]` –
//!   the payload comes from the argument (Codex `notify`) or stdin
//!   (Claude Code hooks, Antigravity hooks);
//! * `task complete <task> [--summary TEXT] [--pr URL]`;
//! * `task block <task> [--reason TEXT]`.
//!
//! A message is one header line of JSON (`{"kind":"hook",...}`) followed by
//! the free-text body (the hook payload, the summary or the reason). The
//! daemon drains every inbox each tick ([`drain`]) and applies the messages
//! exactly as if the host binary had been run; unparsable files move to
//! `inbox/rejected/` and are reported by `doctor`.
//!
//! The task directory (the whole state dir is simplest) must be visible at
//! the same absolute path inside the container, which is also what keeps
//! `--settings`, the prompt file and the transcript paths valid.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::Provider;
use crate::tmux::shell_quote;

/// The shim, relative to the task directory.
pub const SHIM_RELATIVE: &str = "bin/powerqueue";
/// The inbox, relative to the task directory.
pub const INBOX_RELATIVE: &str = "inbox";
/// Where unparsable messages are moved, relative to the inbox.
pub const REJECTED_RELATIVE: &str = "rejected";
/// Extension of a complete message file.
const MESSAGE_EXT: &str = "msg";
/// The per-inbox counter that numbers messages (`<seq>-<epoch>-<pid>-<kind>.msg`),
/// so draining by file name applies them in the order they were written.
const SEQ_FILE: &str = ".seq";

/// `<task dir>/bin/powerqueue`.
pub fn shim_path(task_dir: &Path) -> PathBuf {
    task_dir.join(SHIM_RELATIVE)
}

/// `<task dir>/inbox`.
pub fn inbox_dir(task_dir: &Path) -> PathBuf {
    task_dir.join(INBOX_RELATIVE)
}

/// The header line of a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InboxHeader {
    /// A provider hook; the body is the raw JSON payload.
    Hook {
        #[serde(default = "default_provider")]
        provider: Provider,
        /// Task id (or key / id prefix, as `powerqueue task` accepts).
        task: String,
        #[serde(default)]
        session: Option<String>,
        event: String,
    },
    /// `task complete`; the body is the summary (may be empty).
    Complete {
        task: String,
        #[serde(default)]
        pr: Option<String>,
        #[serde(default)]
        session: Option<String>,
    },
    /// `task block`; the body is the reason (may be empty).
    Block {
        task: String,
        #[serde(default)]
        session: Option<String>,
    },
}

fn default_provider() -> Provider {
    Provider::Claude
}

impl InboxHeader {
    /// The task reference the message names.
    pub fn task(&self) -> &str {
        match self {
            InboxHeader::Hook { task, .. } | InboxHeader::Complete { task, .. } | InboxHeader::Block { task, .. } => task,
        }
    }

    /// The session id the shim saw (`POWERQUEUE_SESSION_ID`, or `--session`).
    pub fn session(&self) -> Option<uuid::Uuid> {
        match self {
            InboxHeader::Hook { session, .. } | InboxHeader::Complete { session, .. } | InboxHeader::Block { session, .. } => {
                session.as_deref().and_then(|s| uuid::Uuid::parse_str(s).ok())
            }
        }
    }

    /// `hook` / `complete` / `block`.
    pub fn kind(&self) -> &'static str {
        match self {
            InboxHeader::Hook { .. } => "hook",
            InboxHeader::Complete { .. } => "complete",
            InboxHeader::Block { .. } => "block",
        }
    }
}

/// One drained message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxMessage {
    pub header: InboxHeader,
    /// Payload, summary or reason, without the trailing newline.
    pub body: String,
}

/// Parse a message file's text. Fails on a missing or malformed header.
pub fn parse_message(text: &str) -> Result<InboxMessage> {
    let (first, rest) = match text.split_once('\n') {
        Some((f, r)) => (f, r),
        None => (text, ""),
    };
    let header: InboxHeader = serde_json::from_str(first.trim()).context("inbox message header")?;
    let body = rest.strip_suffix('\n').unwrap_or(rest).to_string();
    Ok(InboxMessage { header, body })
}

/// Render a message the way the shim does (tests and tools that want to
/// talk to the daemon without the shell script).
pub fn render_message(header: &InboxHeader, body: &str) -> String {
    let mut s = serde_json::to_string(header).expect("inbox header serialises");
    s.push('\n');
    s.push_str(body);
    s.push('\n');
    s
}

/// Write `header` + `body` as a new message in `inbox` (created if needed),
/// atomically, numbered from the inbox's `.seq` counter like the shim does.
/// Returns the file written.
pub fn write_message(inbox: &Path, header: &InboxHeader, body: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(inbox).with_context(|| format!("cannot create {}", inbox.display()))?;
    let seq_file = inbox.join(SEQ_FILE);
    let n: u64 = std::fs::read_to_string(&seq_file).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0) + 1;
    std::fs::write(&seq_file, format!("{n}\n")).with_context(|| format!("cannot write {}", seq_file.display()))?;
    let name = format!("{n:08}-{}-{}-{}.{MESSAGE_EXT}", chrono::Utc::now().timestamp(), std::process::id(), header.kind());
    let tmp = inbox.join(format!(".{name}.tmp"));
    let path = inbox.join(name);
    std::fs::write(&tmp, render_message(header, body)).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot rename {} to {}", tmp.display(), path.display()))?;
    Ok(path)
}

/// Every complete message in `inbox`, oldest first (by file name: the shim
/// names files `<seq>-<epoch>-<pid>-<kind>.msg` with a zero-padded
/// per-inbox sequence number), each with the result of parsing it.
/// Temporary files, dot-files (including the `.seq` counter) and
/// directories are skipped. A missing inbox is empty.
pub fn drain(inbox: &Path) -> Result<Vec<(PathBuf, Result<InboxMessage>)>> {
    let entries = match std::fs::read_dir(inbox) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", inbox.display())),
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|e| e.to_str()) == Some(MESSAGE_EXT)
                && !p.file_name().and_then(|n| n.to_str()).unwrap_or(".").starts_with('.')
        })
        .collect();
    files.sort();
    let mut out = Vec::with_capacity(files.len());
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))
            .and_then(|text| parse_message(&text));
        out.push((path, parsed));
    }
    Ok(out)
}

/// Move an unparsable message to `<inbox>/rejected/` so it is not retried
/// every tick but stays available for inspection.
pub fn reject(path: &Path) -> Result<PathBuf> {
    let inbox = path.parent().ok_or_else(|| anyhow::anyhow!("{} has no parent", path.display()))?;
    let dir = inbox.join(REJECTED_RELATIVE);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let name = path.file_name().ok_or_else(|| anyhow::anyhow!("{} has no file name", path.display()))?;
    let dest = dir.join(name);
    std::fs::rename(path, &dest).with_context(|| format!("cannot move {} to {}", path.display(), dest.display()))?;
    Ok(dest)
}

/// Rejected messages under every task directory in `tasks_dir` (for
/// `doctor`): `(task dir name, count)`.
pub fn rejected_counts(tasks_dir: &Path) -> Vec<(String, usize)> {
    let Ok(entries) = std::fs::read_dir(tasks_dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let dir = entry.path().join(INBOX_RELATIVE).join(REJECTED_RELATIVE);
        let Ok(files) = std::fs::read_dir(&dir) else { continue };
        let n = files.filter_map(|f| f.ok()).filter(|f| f.path().is_file()).count();
        if n > 0 {
            out.push((entry.file_name().to_string_lossy().to_string(), n));
        }
    }
    out.sort();
    out
}

/// The shim script for a task: see the module docs. `inbox` is embedded as
/// an absolute path (`POWERQUEUE_INBOX` overrides it), so the script works
/// wherever it is invoked from, as long as the task directory is visible at
/// the same path.
pub fn shim_script(task_key: &str, inbox: &Path) -> String {
    let inbox_q = shell_quote(&inbox.to_string_lossy());
    let key_q = shell_quote(task_key);
    // Everything below is POSIX sh: dash, busybox ash and bash all run it.
    format!(
        r#"#!/bin/sh
# powerqueue shim for task {key_q}, generated by powerqueue. Do not edit.
#
# Stands in for the `powerqueue` binary where the daemon's own binary cannot
# run (inside a container). `task complete`, `task block` and `hook` each
# write one message into the task inbox; the daemon drains it on its next
# tick. Nothing else is available here.
set -u
inbox="${{POWERQUEUE_INBOX:-}}"
[ -n "$inbox" ] || inbox={inbox_q}

die() {{ echo "powerqueue (shim): $*" >&2; exit 2; }}
# A plain token: letters, digits, _ . : - (ids, event names, providers).
token() {{ case "$2" in ''|*[!A-Za-z0-9_.:-]*) die "invalid $1 \`$2\`" ;; esac; }}
# A task reference: a token, plus / and # for GitHub keys (owner/repo#7).
task_ref() {{ case "$2" in ''|*[!A-Za-z0-9_.:/#-]*) die "invalid $1 \`$2\`" ;; esac; }}
url() {{ case "$2" in ''|*[!A-Za-z0-9_.:/#?=\&%+@~-]*) die "invalid $1 \`$2\`" ;; esac; }}
session_json() {{
  s="${{POWERQUEUE_SESSION_ID:-}}"
  case "$s" in ''|*[!A-Za-z0-9-]*) echo null ;; *) echo "\"$s\"" ;; esac
}}
# emit <header json> <kind>: body on stdin; written to a temp name, then
# renamed. Files are numbered from the inbox's .seq counter so the daemon
# applies them in the order they were written (a Stop hook after `task
# complete --pr` must not overtake it). Callers check the status: in a
# pipeline, `die` only ends this function's subshell.
emit() {{
  mkdir -p "$inbox" || die "cannot create $inbox"
  n=$(cat "$inbox/.seq" 2>/dev/null || echo 0)
  case "$n" in ''|*[!0-9]*) n=0 ;; esac
  n=$((n + 1))
  echo "$n" > "$inbox/.seq" || die "cannot write to $inbox"
  name="$(printf '%08d' "$n")-$(date +%s)-$$-$2"
  tmp="$inbox/.$name.tmp"
  {{ printf '%s\n' "$1"; cat; }} > "$tmp" && mv "$tmp" "$inbox/$name.msg" || die "cannot write to $inbox"
}}

cmd="${{1:-}}"
[ $# -gt 0 ] && shift
case "$cmd" in
  hook)
    # Never fail a hook: a non-zero exit would stop the agent's turn.
    (
      provider=claude; task=; session=; event=; payload=; has_payload=0
      while [ $# -gt 0 ]; do
        case "$1" in
          --provider) provider="${{2:-}}"; shift; [ $# -gt 0 ] && shift ;;
          --task) task="${{2:-}}"; shift; [ $# -gt 0 ] && shift ;;
          --session) session="${{2:-}}"; shift; [ $# -gt 0 ] && shift ;;
          --event) event="${{2:-}}"; shift; [ $# -gt 0 ] && shift ;;
          --) shift; if [ $# -gt 0 ]; then payload="$1"; has_payload=1; fi; break ;;
          -*) shift ;;
          *) payload="$1"; has_payload=1; shift ;;
        esac
      done
      token provider "$provider"; task_ref task "$task"; token event "$event"
      if [ -n "$session" ]; then token session "$session"; session_json="\"$session\""; else session_json="$(session_json)"; fi
      header="{{\"kind\":\"hook\",\"provider\":\"$provider\",\"task\":\"$task\",\"session\":$session_json,\"event\":\"$event\"}}"
      if [ "$has_payload" = 1 ]; then printf '%s\n' "$payload" | emit "$header" hook; else emit "$header" hook; fi
    ) || echo "powerqueue (shim): hook not delivered" >&2
    exit 0
    ;;
  task)
    sub="${{1:-}}"; [ $# -gt 0 ] && shift
    id="${{1:-}}"; [ $# -gt 0 ] && shift
    case "$sub" in
      complete)
        usage="usage: powerqueue task complete <task> [--summary TEXT] [--pr URL]"
        summary=; pr=
        while [ $# -gt 0 ]; do
          case "$1" in
            --summary) summary="${{2:-}}"; shift; [ $# -gt 0 ] && shift ;;
            --summary=*) summary="${{1#--summary=}}"; shift ;;
            --pr) pr="${{2:-}}"; shift; [ $# -gt 0 ] && shift ;;
            --pr=*) pr="${{1#--pr=}}"; shift ;;
            *) die "unknown argument \`$1\` ($usage)" ;;
          esac
        done
        [ -n "$id" ] || die "$usage"
        task_ref task "$id"
        if [ -n "$pr" ]; then url pr "$pr"; pr_json="\"$pr\""; else pr_json=null; fi
        printf '%s' "$summary" | emit "{{\"kind\":\"complete\",\"task\":\"$id\",\"pr\":$pr_json,\"session\":$(session_json)}}" complete || exit 2
        if [ -n "$pr" ]; then
          echo "in review: $id ($pr); the daemon releases its slot and worktree and watches the PR"
        else
          echo "done: $id marked completed; the daemon will clean up its worktree"
        fi
        ;;
      block)
        usage="usage: powerqueue task block <task> [--reason TEXT]"
        reason=
        while [ $# -gt 0 ]; do
          case "$1" in
            --reason) reason="${{2:-}}"; shift; [ $# -gt 0 ] && shift ;;
            --reason=*) reason="${{1#--reason=}}"; shift ;;
            *) die "unknown argument \`$1\` ($usage)" ;;
          esac
        done
        [ -n "$id" ] || die "$usage"
        task_ref task "$id"
        printf '%s' "$reason" | emit "{{\"kind\":\"block\",\"task\":\"$id\",\"session\":$(session_json)}}" block || exit 2
        echo "blocked: $id needs attention; the daemon has been told"
        ;;
      *) die "only \`task complete\` and \`task block\` are available inside this session" ;;
    esac
    ;;
  --version|-V|version) echo "powerqueue shim for task {key_q}" ;;
  *) die "only \`task complete\`, \`task block\` and \`hook\` are available inside this session (got \`$cmd\`)" ;;
esac
exit 0
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn write_shim(dir: &Path) -> PathBuf {
        let task_dir = dir.join("task");
        let shim = shim_path(&task_dir);
        std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
        std::fs::write(&shim, shim_script("ENG-7", &inbox_dir(&task_dir))).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        shim
    }

    fn run(shim: &Path, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> (i32, String, String) {
        let mut cmd = Command::new("sh");
        cmd.arg(shim).args(args).env_remove("POWERQUEUE_SESSION_ID").envs(env.iter().copied());
        cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        if let Some(text) = stdin {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
        }
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    #[test]
    fn message_round_trip() {
        let header = InboxHeader::Complete { task: "ENG-1".into(), pr: None, session: None };
        let text = render_message(&header, "all done\nsecond line");
        let msg = parse_message(&text).unwrap();
        assert_eq!(msg.header, header);
        assert_eq!(msg.body, "all done\nsecond line");
        assert_eq!(msg.header.task(), "ENG-1");
        assert_eq!(msg.header.kind(), "complete");
        assert!(msg.header.session().is_none());
        let hook = parse_message(r#"{"kind":"hook","task":"t","event":"Stop"}"#).unwrap();
        assert_eq!(
            hook.header,
            InboxHeader::Hook { provider: Provider::Claude, task: "t".into(), session: None, event: "Stop".into() }
        );
        assert_eq!(hook.body, "");
        assert!(parse_message("not json\nbody").is_err());
        assert!(parse_message(r#"{"kind":"dance","task":"t"}"#).is_err());
    }

    #[test]
    fn drain_orders_skips_and_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = inbox_dir(dir.path());
        assert!(drain(&inbox).unwrap().is_empty(), "a missing inbox is empty");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("200-1-hook.msg"), r#"{"kind":"hook","task":"t","event":"Stop"}"#).unwrap();
        std::fs::write(inbox.join("100-1-complete.msg"), "{\"kind\":\"complete\",\"task\":\"t\"}\nsummary\n").unwrap();
        std::fs::write(inbox.join("150-1-bad.msg"), "garbage\n").unwrap();
        std::fs::write(inbox.join(".300-1-hook.msg.tmp"), "partial").unwrap();
        std::fs::write(inbox.join("notes.txt"), "ignored").unwrap();
        std::fs::create_dir_all(inbox.join("rejected")).unwrap();
        let drained = drain(&inbox).unwrap();
        let names: Vec<String> = drained.iter().map(|(p, _)| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["100-1-complete.msg", "150-1-bad.msg", "200-1-hook.msg"]);
        assert_eq!(drained[0].1.as_ref().unwrap().body, "summary");
        assert!(drained[1].1.is_err());
        let moved = reject(&drained[1].0).unwrap();
        assert!(moved.ends_with("rejected/150-1-bad.msg"));
        assert!(!inbox.join("150-1-bad.msg").exists());
        let tasks = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tasks.path().join("t1/inbox/rejected")).unwrap();
        std::fs::write(tasks.path().join("t1/inbox/rejected/x.msg"), "x").unwrap();
        std::fs::create_dir_all(tasks.path().join("t2/inbox")).unwrap();
        assert_eq!(rejected_counts(tasks.path()), vec![("t1".to_string(), 1)]);
        assert!(rejected_counts(Path::new("/definitely/missing")).is_empty());
    }

    #[test]
    fn write_message_is_drained_back() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = inbox_dir(dir.path());
        let header = InboxHeader::Block { task: "ENG-2".into(), session: Some(uuid::Uuid::nil().to_string()) };
        let path = write_message(&inbox, &header, "need creds").unwrap();
        assert!(path.extension().unwrap() == "msg");
        let drained = drain(&inbox).unwrap();
        assert_eq!(drained.len(), 1);
        let msg = drained[0].1.as_ref().unwrap();
        assert_eq!(msg.header, header);
        assert_eq!(msg.header.session(), Some(uuid::Uuid::nil()));
        assert_eq!(msg.body, "need creds");
    }

    #[test]
    fn shim_delivers_hooks_and_task_commands() {
        if which::which("sh").is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let shim = write_shim(dir.path());
        let inbox = inbox_dir(&dir.path().join("task"));
        let sid = uuid::Uuid::new_v4().to_string();

        // Claude-style hook: payload on stdin.
        let (code, _, err) =
            run(&shim, &["hook", "--task", "abc-1", "--session", &sid, "--event", "Stop"], Some(r#"{"a":1}"#), &[]);
        assert_eq!(code, 0, "{err}");
        // Codex-style hook: payload as the last argument, no stdin.
        let (code, _, err) = run(
            &shim,
            &["hook", "--provider", "codex", "--task", "abc-1", "--event", "agent-turn-complete", r#"{"type":"x"}"#],
            None,
            &[],
        );
        assert_eq!(code, 0, "{err}");
        // task complete with a summary and a PR, with the session from the environment.
        let (code, out, err) = run(
            &shim,
            &["task", "complete", "abc-1", "--summary", "all \"done\" here", "--pr", "https://github.com/o/r/pull/7"],
            None,
            &[("POWERQUEUE_SESSION_ID", &sid)],
        );
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("in review"), "{out}");
        // A GitHub task key (owner/repo#7) is a valid reference.
        let (code, out, err) = run(&shim, &["task", "block", "acme/widgets#7", "--reason=waiting on creds"], None, &[]);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("blocked"), "{out}");

        let drained = drain(&inbox).unwrap();
        // Drained in the order the calls were made: the names carry the
        // inbox's sequence number, not the (unordered) pid.
        let names: Vec<String> = drained.iter().map(|(p, _)| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert!(names[0].starts_with("00000001-") && names[3].starts_with("00000004-"), "{names:?}");
        assert_eq!(std::fs::read_to_string(inbox.join(".seq")).unwrap().trim(), "4");
        let msgs: Vec<InboxMessage> = drained.into_iter().map(|(_, m)| m.unwrap()).collect();
        assert_eq!(msgs.len(), 4, "{msgs:?}");
        assert_eq!(msgs.iter().map(|m| m.header.kind()).collect::<Vec<_>>(), ["hook", "hook", "complete", "block"]);
        assert_eq!(msgs[3].header.task(), "acme/widgets#7");
        let hook = msgs.iter().find(|m| matches!(&m.header, InboxHeader::Hook { provider: Provider::Claude, .. })).unwrap();
        assert_eq!(hook.body, r#"{"a":1}"#);
        assert_eq!(hook.header.session().map(|u| u.to_string()), Some(sid.clone()));
        let codex = msgs.iter().find(|m| matches!(&m.header, InboxHeader::Hook { provider: Provider::Codex, .. })).unwrap();
        assert_eq!(codex.body, r#"{"type":"x"}"#);
        assert!(matches!(&codex.header, InboxHeader::Hook { event, session: None, .. } if event == "agent-turn-complete"));
        let complete = msgs.iter().find(|m| matches!(&m.header, InboxHeader::Complete { .. })).unwrap();
        assert_eq!(complete.body, "all \"done\" here");
        assert!(matches!(&complete.header, InboxHeader::Complete { pr: Some(pr), .. } if pr == "https://github.com/o/r/pull/7"));
        assert_eq!(complete.header.session().map(|u| u.to_string()), Some(sid));
        let block = msgs.iter().find(|m| matches!(&m.header, InboxHeader::Block { .. })).unwrap();
        assert_eq!(block.body, "waiting on creds");
    }

    #[test]
    fn shim_refuses_what_it_cannot_do() {
        if which::which("sh").is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let shim = write_shim(dir.path());
        let inbox = inbox_dir(&dir.path().join("task"));
        let (code, _, err) = run(&shim, &["status"], None, &[]);
        assert_eq!(code, 2);
        assert!(err.contains("only"), "{err}");
        let (code, _, err) = run(&shim, &["task", "complete"], None, &[]);
        assert_eq!(code, 2);
        assert!(err.contains("usage"), "{err}");
        let (code, _, err) = run(&shim, &["task", "complete", "abc 1"], None, &[]);
        assert_eq!(code, 2);
        assert!(err.contains("invalid task"), "{err}");
        let (code, _, err) = run(&shim, &["task", "complete", "abc-1", "--pr", "javascript:alert(1)\""], None, &[]);
        assert_eq!(code, 2, "{err}");
        // A hook never fails, even when it is malformed.
        let (code, _, err) = run(&shim, &["hook", "--task", "bad id"], Some("{}"), &[]);
        assert_eq!(code, 0);
        assert!(err.contains("not delivered"), "{err}");
        let (code, out, _) = run(&shim, &["--version"], None, &[]);
        assert_eq!(code, 0);
        assert!(out.contains("shim"), "{out}");
        assert!(drain(&inbox).unwrap().is_empty(), "nothing was written");
        // An inbox override wins over the embedded path.
        let other = dir.path().join("other-inbox");
        let (code, _, _) = run(&shim, &["task", "complete", "abc-1"], None, &[("POWERQUEUE_INBOX", &other.to_string_lossy())]);
        assert_eq!(code, 0);
        assert_eq!(drain(&other).unwrap().len(), 1);
        // An inbox that cannot be written fails `task complete|block` loudly
        // (exit 2, no success line); a hook still exits 0 and says so.
        let unwritable = dir.path().join("a-file");
        std::fs::write(&unwritable, "not a directory").unwrap();
        let bad = unwritable.join("inbox").to_string_lossy().to_string();
        let (code, out, err) = run(&shim, &["task", "complete", "abc-1", "--summary", "x"], None, &[("POWERQUEUE_INBOX", &bad)]);
        assert_eq!(code, 2, "{out}{err}");
        assert!(!out.contains("done"), "{out}");
        assert!(err.contains("cannot"), "{err}");
        let (code, _, err) = run(&shim, &["task", "block", "abc-1"], None, &[("POWERQUEUE_INBOX", &bad)]);
        assert_eq!(code, 2, "{err}");
        let (code, _, err) =
            run(&shim, &["hook", "--task", "abc-1", "--event", "Stop"], Some("{}"), &[("POWERQUEUE_INBOX", &bad)]);
        assert_eq!(code, 0);
        assert!(err.contains("not delivered"), "{err}");
    }

    #[test]
    fn write_message_numbers_files_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = inbox_dir(dir.path());
        let a = write_message(&inbox, &InboxHeader::Complete { task: "t".into(), pr: None, session: None }, "first").unwrap();
        let b = write_message(&inbox, &InboxHeader::Block { task: "t".into(), session: None }, "second").unwrap();
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("00000001-"), "{}", a.display());
        assert!(b.file_name().unwrap().to_string_lossy().starts_with("00000002-"), "{}", b.display());
        let bodies: Vec<String> = drain(&inbox).unwrap().into_iter().map(|(_, m)| m.unwrap().body).collect();
        assert_eq!(bodies, ["first", "second"]);
    }
}
