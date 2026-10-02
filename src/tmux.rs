//! tmux integration.
//!
//! One tmux *session* (default `powerqueue`) hosts one *window* per task.
//! Windows are named after the task key so `powerqueue attach ENG-123` is
//! simply `tmux select-window`. Panes keep `remain-on-exit` so a crashed
//! Claude process leaves its last screen for inspection.
//!
//! Every operation shells out to the `tmux` binary with explicit arguments
//! (never through a shell), and every failure carries tmux's stderr text.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
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

/// Format string used wherever we need a window id, pane id and pane pid.
const IDS_FORMAT: &str = "#{window_id} #{pane_id} #{pane_pid}";

/// Format string for [`Tmux::list_panes`]: tab-separated so names with spaces survive.
const PANES_FORMAT: &str =
    "#{window_id}\t#{window_name}\t#{pane_id}\t#{pane_pid}\t#{pane_dead}\t#{pane_dead_status}\t#{pane_current_command}";

/// Name of the session hook that makes new windows keep their pane after the
/// command exits. Session-scoped, so it never touches the user's own sessions.
const REMAIN_ON_EXIT_HOOK: &str = "after-new-window";

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
    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.binary);
        if let Some(sock) = &self.socket_name {
            c.arg("-L").arg(sock);
        }
        c
    }

    /// The `tmux [-L socket]` prefix as displayed to users (`attach --print`).
    pub fn command_prefix(&self) -> Vec<String> {
        let mut v = vec![self.binary.clone()];
        if let Some(sock) = &self.socket_name {
            v.push("-L".to_string());
            v.push(sock.clone());
        }
        v
    }

    /// Run `tmux <args>` and return its raw output. Fails only when the
    /// binary cannot be started; callers inspect the exit status.
    fn output(&self, args: &[&str]) -> Result<Output> {
        let mut cmd = self.command();
        cmd.args(args);
        cmd.stdin(std::process::Stdio::null());
        cmd.output().with_context(|| format!("cannot run `{}` (is tmux installed?)", self.describe(args)))
    }

    /// Run `tmux <args>`; return trimmed stdout or an error carrying stderr.
    fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.output(args)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!(
                "`{}` failed ({}): {}",
                self.describe(args),
                out.status,
                if stderr.is_empty() { "<no output>" } else { &stderr }
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end_matches(['\n', '\r']).to_string())
    }

    /// Human-readable command line for error messages.
    fn describe(&self, args: &[&str]) -> String {
        let mut parts = self.command_prefix();
        parts.extend(args.iter().map(|a| shell_quote(a)));
        parts.join(" ")
    }

    /// `tmux -V`, or an error if tmux is missing.
    pub fn version(&self) -> Result<String> {
        Ok(self.run(&["-V"])?.trim().to_string())
    }

    /// Whether a session with exactly this name exists (`=` forces an exact
    /// match). A missing server counts as "no session".
    pub fn has_session(&self, session: &str) -> Result<bool> {
        let target = format!("={session}");
        let out = self.output(&["has-session", "-t", &target])?;
        Ok(out.status.success())
    }

    /// Create the hosting session (detached) if needed. The first window is a
    /// plain shell named `powerqueue` so the session survives task windows closing.
    pub fn ensure_session(&self, session: &str, cwd: &Path) -> Result<()> {
        if self.has_session(session)? {
            tracing::debug!(session, "tmux session already exists");
            return Ok(());
        }
        let cwd_s = cwd.to_string_lossy();
        self.run(&["new-session", "-d", "-s", session, "-c", &cwd_s, "-n", "powerqueue"])?;
        tracing::info!(session, cwd = %cwd.display(), "created tmux session");
        Ok(())
    }

    /// Create a window running `shell_command` (via `sh -c`) in `cwd`.
    /// Sets `remain-on-exit` when requested and returns ids + pane pid.
    ///
    /// `remain-on-exit` is a window option that can only be set once the
    /// window exists, and a command that fails instantly would destroy the
    /// window before we get there. To close that race we install a
    /// session-scoped `after-new-window` hook that turns the option on as
    /// part of the `new-window` command itself, then pin the per-window value
    /// explicitly (which is also how windows opt *out* once the hook exists).
    pub fn new_window(
        &self,
        session: &str,
        name: &str,
        cwd: &Path,
        shell_command: &str,
        remain_on_exit: bool,
    ) -> Result<WindowInfo> {
        if remain_on_exit {
            self.run(&["set-hook", "-t", session, REMAIN_ON_EXIT_HOOK, "set-option -w remain-on-exit on"])?;
        }
        let target = format!("{session}:");
        let cwd_s = cwd.to_string_lossy();
        let line = self.run(&[
            "new-window",
            "-d",
            "-t",
            &target,
            "-n",
            name,
            "-c",
            &cwd_s,
            "-P",
            "-F",
            IDS_FORMAT,
            "--",
            "sh",
            "-c",
            shell_command,
        ])?;
        let info = parse_window_info(&line)?;
        let value = if remain_on_exit { "on" } else { "off" };
        let pinned = self.run(&["set-option", "-w", "-t", &info.window_id, "remain-on-exit", value]);
        match pinned {
            Ok(_) => {}
            // Without remain-on-exit a command that exited already took its
            // window with it; that is the requested behaviour, not an error.
            Err(_) if !remain_on_exit => {}
            Err(e) => return Err(e),
        }
        tracing::info!(session, window = %info.window_id, pane = %info.pane_id, pid = info.pane_pid, name, "created tmux window");
        Ok(info)
    }

    /// Re-run a command in an existing (dead) pane: `respawn-pane -k`.
    pub fn respawn_pane(&self, pane_id: &str, cwd: &Path, shell_command: &str) -> Result<WindowInfo> {
        let cwd_s = cwd.to_string_lossy();
        self.run(&["respawn-pane", "-k", "-c", &cwd_s, "-t", pane_id, "--", "sh", "-c", shell_command])?;
        let line = self.run(&["display-message", "-p", "-t", pane_id, "-F", IDS_FORMAT])?;
        let info = parse_window_info(&line)?;
        tracing::info!(pane = %info.pane_id, pid = info.pane_pid, "respawned tmux pane");
        Ok(info)
    }

    /// Every pane in every window of `session`. A missing session (or no
    /// server at all) yields an empty list rather than an error.
    pub fn list_panes(&self, session: &str) -> Result<Vec<PaneInfo>> {
        if !self.has_session(session)? {
            return Ok(Vec::new());
        }
        let target = format!("={session}");
        let text = self.run(&["list-panes", "-s", "-t", &target, "-F", PANES_FORMAT])?;
        text.lines().filter(|l| !l.trim().is_empty()).map(parse_pane_line).collect()
    }

    pub fn find_pane(&self, session: &str, pane_id: &str) -> Result<Option<PaneInfo>> {
        Ok(self.list_panes(session)?.into_iter().find(|p| p.pane_id == pane_id))
    }

    /// Kill a window by id (`@3`) or name.
    pub fn kill_window(&self, window_id: &str) -> Result<()> {
        self.run(&["kill-window", "-t", window_id])?;
        tracing::info!(window = window_id, "killed tmux window");
        Ok(())
    }

    /// Kill the whole session (exact name match).
    pub fn kill_session(&self, session: &str) -> Result<()> {
        let target = format!("={session}");
        self.run(&["kill-session", "-t", &target])?;
        tracing::info!(session, "killed tmux session");
        Ok(())
    }

    /// Send literal text followed by Enter to a pane.
    pub fn send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        self.run(&["send-keys", "-t", pane_id, "-l", "--", text])?;
        self.run(&["send-keys", "-t", pane_id, "Enter"])?;
        Ok(())
    }

    /// Send a key name such as `C-c` or `Escape`.
    pub fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        self.run(&["send-keys", "-t", pane_id, key])?;
        Ok(())
    }

    /// Last `lines` lines of the pane's screen (including scrollback), with
    /// trailing blank lines removed.
    pub fn capture_pane(&self, pane_id: &str, lines: u32) -> Result<String> {
        let start = format!("-{lines}");
        let text = self.run(&["capture-pane", "-p", "-t", pane_id, "-S", &start])?;
        Ok(text.trim_end_matches(['\n', '\r', ' ']).to_string())
    }

    /// Argument vector to attach a terminal to `window` of `session`
    /// (switch-client when already inside tmux, attach-session otherwise).
    pub fn attach_args(&self, session: &str, window_id: Option<&str>) -> Vec<String> {
        Self::attach_args_for(Self::inside_tmux(), session, window_id)
    }

    /// [`Tmux::attach_args`] with the "inside tmux" decision made explicit (testable).
    pub fn attach_args_for(inside_tmux: bool, session: &str, window_id: Option<&str>) -> Vec<String> {
        if inside_tmux {
            let target = match window_id {
                Some(w) => format!("{session}:{w}"),
                None => session.to_string(),
            };
            vec!["switch-client".to_string(), "-t".to_string(), target]
        } else {
            vec!["attach-session".to_string(), "-t".to_string(), session.to_string()]
        }
    }

    /// Replace the current process with `tmux attach` (unix) or spawn and wait.
    /// When a window is given it is selected first so the terminal lands on it.
    pub fn attach(&self, session: &str, window_id: Option<&str>) -> Result<()> {
        if let Some(w) = window_id {
            let target = format!("{session}:{w}");
            self.run(&["select-window", "-t", &target])?;
        }
        let args = self.attach_args(session, window_id);
        let mut cmd = self.command();
        cmd.args(&args);
        tracing::info!(session, window = window_id.unwrap_or("-"), "attaching to tmux");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = cmd.exec();
            Err(anyhow::Error::from(err)
                .context(format!("cannot exec `{}`", self.describe(&args.iter().map(String::as_str).collect::<Vec<_>>()))))
        }
        #[cfg(not(unix))]
        {
            let status = cmd.status().with_context(|| format!("cannot run `{}`", self.binary))?;
            if !status.success() {
                bail!("tmux attach exited with {status}");
            }
            Ok(())
        }
    }

    /// True if the current process is running inside tmux (`$TMUX` set).
    pub fn inside_tmux() -> bool {
        std::env::var_os("TMUX").is_some()
    }
}

/// Parse `#{window_id} #{pane_id} #{pane_pid}`.
fn parse_window_info(line: &str) -> Result<WindowInfo> {
    let mut parts = line.split_whitespace();
    let (Some(window_id), Some(pane_id), Some(pid)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("unexpected tmux output `{line}` (expected `window_id pane_id pane_pid`)");
    };
    let pane_pid = pid.parse::<u32>().with_context(|| format!("bad pane pid `{pid}` in tmux output `{line}`"))?;
    Ok(WindowInfo { window_id: window_id.to_string(), pane_id: pane_id.to_string(), pane_pid })
}

/// Parse one tab-separated line in [`PANES_FORMAT`].
fn parse_pane_line(line: &str) -> Result<PaneInfo> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() < 7 {
        bail!("unexpected `tmux list-panes` line `{line}` ({} fields, expected 7)", fields.len());
    }
    let pane_pid = fields[3].trim().parse::<u32>().with_context(|| format!("bad pane pid in `{line}`"))?;
    let dead = matches!(fields[4].trim(), "1" | "on" | "yes");
    let dead_status = fields[5].trim().parse::<i32>().ok();
    Ok(PaneInfo {
        window_id: fields[0].trim().to_string(),
        window_name: fields[1].to_string(),
        pane_id: fields[2].trim().to_string(),
        pane_pid,
        dead,
        dead_status: if dead { dead_status } else { None },
        // Extra tabs (a command name containing a tab) are folded back in.
        current_command: fields[6..].join("\t").trim().to_string(),
    })
}

/// Quote a string for `sh -c`.
pub fn shell_quote(s: &str) -> String {
    shlex::try_quote(s).map(|c| c.into_owned()).unwrap_or_else(|_| format!("'{}'", s.replace('\'', "'\\''")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_window_info() {
        let w = parse_window_info("@3 %7 12345\n").unwrap();
        assert_eq!(w, WindowInfo { window_id: "@3".into(), pane_id: "%7".into(), pane_pid: 12345 });
        assert!(parse_window_info("@3 %7").is_err());
        assert!(parse_window_info("@3 %7 abc").is_err());
    }

    #[test]
    fn parses_pane_lines() {
        let alive = parse_pane_line("@0\tpowerqueue\t%0\t100\t0\t\tzsh").unwrap();
        assert_eq!(alive.window_name, "powerqueue");
        assert!(!alive.dead);
        assert_eq!(alive.dead_status, None);
        assert_eq!(alive.current_command, "zsh");

        let dead = parse_pane_line("@2\teng-123\t%2\t101\t1\t3\tsh").unwrap();
        assert!(dead.dead);
        assert_eq!(dead.dead_status, Some(3));
        assert_eq!(dead.pane_pid, 101);

        let no_status = parse_pane_line("@2\tx\t%2\t101\t1\t\tsh").unwrap();
        assert!(no_status.dead);
        assert_eq!(no_status.dead_status, None);

        assert!(parse_pane_line("@2\tx\t%2").is_err());
        assert!(parse_pane_line("@2\tx\t%2\tnope\t0\t\tsh").is_err());
    }

    #[test]
    fn attach_args_depend_on_environment() {
        assert_eq!(Tmux::attach_args_for(true, "pq", Some("@3")), vec!["switch-client", "-t", "pq:@3"]);
        assert_eq!(Tmux::attach_args_for(true, "pq", None), vec!["switch-client", "-t", "pq"]);
        assert_eq!(Tmux::attach_args_for(false, "pq", Some("@3")), vec!["attach-session", "-t", "pq"]);
    }

    #[test]
    fn command_prefix_includes_socket() {
        let t = Tmux::new("tmux", Some("sock".into()));
        assert_eq!(t.command_prefix(), vec!["tmux", "-L", "sock"]);
        assert_eq!(Tmux::new("tmux", None).command_prefix(), vec!["tmux"]);
    }

    #[test]
    fn shell_quote_is_safe() {
        assert_eq!(shell_quote("plain"), "plain");
        assert_eq!(shell_quote("it's"), "\"it's\"");
        assert!(shell_quote("a b").starts_with(['\'', '"']));
    }
}
