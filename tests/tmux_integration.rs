//! Exercises `powerqueue::tmux::Tmux` against a real tmux server.
//!
//! Every test runs on its own private socket (`tmux -L powerqueue-test-<pid>-<random>`)
//! so it never touches the user's tmux, and the server is killed when the
//! guard drops (also on panic). Skipped when `tmux` is not on PATH.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use powerqueue::tmux::{PaneInfo, Tmux};

/// Kills the private tmux server on drop.
struct Server {
    tmux: Tmux,
    socket: String,
}

impl Server {
    fn start() -> Option<Self> {
        if which::which("tmux").is_err() {
            eprintln!("skipping: tmux not on PATH");
            return None;
        }
        let socket = format!("powerqueue-test-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
        Some(Self { tmux: Tmux::new("tmux", Some(socket.clone())), socket })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // tmux leaves the socket file behind after kill-server; remove it so
        // repeated test runs do not litter /tmp/tmux-<uid>/. `start-server`
        // first: a test may already have killed the last session (and with
        // it the server), and only a live server can report its socket path.
        let socket_path = std::process::Command::new("tmux")
            .args(["-L", &self.socket, "start-server", ";", "display-message", "-p", "#{socket_path}"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let _ = std::process::Command::new("tmux").args(["-L", &self.socket, "kill-server"]).output();
        if let Some(p) = socket_path.filter(|p| !p.is_empty()) {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn cwd() -> PathBuf {
    std::env::temp_dir()
}

/// Poll until `pred` holds for the pane or the timeout elapses.
fn wait_for_pane(tmux: &Tmux, session: &str, pane_id: &str, pred: impl Fn(&PaneInfo) -> bool) -> Option<PaneInfo> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(p) = tmux.find_pane(session, pane_id).unwrap()
            && pred(&p)
        {
            return Some(p);
        }
        if Instant::now() > deadline {
            return tmux.find_pane(session, pane_id).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll until the pane's screen contains `needle`.
fn wait_for_output(tmux: &Tmux, pane_id: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = tmux.capture_pane(pane_id, 200).unwrap();
        if text.contains(needle) || Instant::now() > deadline {
            return text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn version_reports_tmux() {
    let Some(srv) = Server::start() else { return };
    let v = srv.tmux.version().unwrap();
    assert!(v.starts_with("tmux "), "{v}");
}

#[test]
fn ensure_session_is_idempotent() {
    let Some(srv) = Server::start() else { return };
    let t = &srv.tmux;
    assert!(!t.has_session("pq").unwrap());
    assert_eq!(t.list_panes("pq").unwrap(), Vec::<PaneInfo>::new());
    t.ensure_session("pq", &cwd()).unwrap();
    assert!(t.has_session("pq").unwrap());
    t.ensure_session("pq", &cwd()).unwrap();
    let panes = t.list_panes("pq").unwrap();
    assert_eq!(panes.len(), 1, "{panes:?}");
    assert_eq!(panes[0].window_name, "powerqueue");
    // `=` forces exact matching: a prefix must not match.
    assert!(!t.has_session("p").unwrap());
    t.kill_session("pq").unwrap();
    assert!(!t.has_session("pq").unwrap());
}

#[test]
fn new_window_runs_command_and_is_listed() {
    let Some(srv) = Server::start() else { return };
    let t = &srv.tmux;
    t.ensure_session("pq", &cwd()).unwrap();
    let w = t.new_window("pq", "eng-1", &cwd(), "echo hi; sleep 30", true).unwrap();
    assert!(w.window_id.starts_with('@'));
    assert!(w.pane_id.starts_with('%'));
    assert!(w.pane_pid > 0);

    let pane = wait_for_pane(t, "pq", &w.pane_id, |_| true).expect("pane listed");
    assert_eq!(pane.window_id, w.window_id);
    assert_eq!(pane.window_name, "eng-1");
    assert_eq!(pane.pane_pid, w.pane_pid);
    assert!(!pane.dead);
    assert_eq!(pane.dead_status, None);

    let text = wait_for_output(t, &w.pane_id, "hi");
    assert!(text.contains("hi"), "{text:?}");
    assert!(!text.ends_with('\n'));

    t.kill_window(&w.window_id).unwrap();
    assert!(t.find_pane("pq", &w.pane_id).unwrap().is_none());
    // The hosting window survives.
    assert_eq!(t.list_panes("pq").unwrap().len(), 1);
}

#[test]
fn dead_pane_reports_exit_status_and_can_be_respawned() {
    let Some(srv) = Server::start() else { return };
    let t = &srv.tmux;
    t.ensure_session("pq", &cwd()).unwrap();
    let w = t.new_window("pq", "dies", &cwd(), "exit 3", true).unwrap();

    let pane = wait_for_pane(t, "pq", &w.pane_id, |p| p.dead).expect("pane kept by remain-on-exit");
    assert!(pane.dead, "{pane:?}");
    assert_eq!(pane.dead_status, Some(3));

    let again = t.respawn_pane(&w.pane_id, &cwd(), "echo back; sleep 30").unwrap();
    assert_eq!(again.window_id, w.window_id);
    assert_eq!(again.pane_id, w.pane_id);
    assert_ne!(again.pane_pid, w.pane_pid);
    let pane = wait_for_pane(t, "pq", &w.pane_id, |p| !p.dead).unwrap();
    assert!(!pane.dead);
    let text = wait_for_output(t, &w.pane_id, "back");
    assert!(text.contains("back"), "{text:?}");
}

#[test]
fn window_without_remain_on_exit_disappears() {
    let Some(srv) = Server::start() else { return };
    let t = &srv.tmux;
    t.ensure_session("pq", &cwd()).unwrap();
    let w = t.new_window("pq", "gone", &cwd(), "exit 0", false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while t.find_pane("pq", &w.pane_id).unwrap().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(t.find_pane("pq", &w.pane_id).unwrap().is_none());
}

#[test]
fn send_text_reaches_the_pane() {
    let Some(srv) = Server::start() else { return };
    let t = &srv.tmux;
    t.ensure_session("pq", &cwd()).unwrap();
    // Observe cat's exit directly rather than the platform's sh wrapper.
    let w = t.new_window("pq", "cat", &cwd(), "exec cat", true).unwrap();
    let pane = wait_for_pane(t, "pq", &w.pane_id, |p| p.current_command == "cat").unwrap();
    assert_eq!(pane.current_command, "cat", "{pane:?}");
    t.send_text(&w.pane_id, "-n hello -- world").unwrap();
    let text = wait_for_output(t, &w.pane_id, "hello -- world");
    assert!(text.contains("-n hello -- world"), "{text:?}");
    t.send_key(&w.pane_id, "C-d").unwrap();
    // The pane dies as soon as the PTY closes. Its exit status is a separate
    // event: tmux 3.4 on Linux regularly loses the SIGCHLD of a child that
    // replaced the pane's shell with `exec` (the process stays a zombie until
    // the next SIGCHLD from any other child), so the status may arrive
    // seconds later or not at all. `dead_pane_reports_exit_status_and_can_be_respawned`
    // covers the status with a plain `exit 3`; here the status is only
    // checked when tmux did report it.
    let pane = wait_for_pane(t, "pq", &w.pane_id, |p| p.dead).unwrap();
    assert!(pane.dead, "{pane:?}");
    assert!(pane.dead_status.is_none_or(|s| s == 0), "{pane:?}");
}

#[test]
fn errors_include_tmux_stderr() {
    let Some(srv) = Server::start() else { return };
    let t = &srv.tmux;
    t.ensure_session("pq", &cwd()).unwrap();
    let err = t.kill_window("@999").unwrap_err().to_string();
    assert!(err.contains("kill-window"), "{err}");
    assert!(err.contains("can't find window") || err.contains("no such window"), "{err}");
    let err = t.capture_pane("%999", 10).unwrap_err().to_string();
    assert!(err.contains("%999"), "{err}");
}

#[test]
fn missing_binary_is_an_error() {
    let t = Tmux::new("/nonexistent/powerqueue-tmux", None);
    let err = t.version().unwrap_err().to_string();
    assert!(err.contains("is tmux installed"), "{err}");
}

/// A window named like the powerqueue session in *another* session (tmux's
/// automatic-rename gives the window running `powerqueue run` that very
/// name) must not hijack the lookups: `list-panes`, `set-hook` and
/// `new-window` take window/pane targets, which tmux resolves against the
/// current session's windows before trying session names.
#[test]
fn a_window_named_like_the_session_elsewhere_does_not_hijack_lookups() {
    let Some(srv) = Server::start() else { return };
    let t = &srv.tmux;
    t.ensure_session("pq", &cwd()).unwrap();
    let w = t.new_window("pq", "eng-1", &cwd(), "sleep 30", true).unwrap();
    // The operator's own session, most recently active, with a window named "pq".
    t.ensure_session("other", &cwd()).unwrap();
    let decoy = t.new_window("other", "pq", &cwd(), "sleep 30", false).unwrap();

    let panes = t.list_panes("pq").unwrap();
    assert!(panes.iter().any(|p| p.pane_id == w.pane_id), "own pane missing: {panes:?}");
    assert!(panes.iter().all(|p| p.pane_id != decoy.pane_id), "decoy session listed: {panes:?}");
    assert!(t.find_pane("pq", &w.pane_id).unwrap().is_some());

    // A second window in "pq" lands in "pq" and keeps its pane after exit…
    let w2 = t.new_window("pq", "eng-2", &cwd(), "exit 0", true).unwrap();
    let pane = wait_for_pane(t, "pq", &w2.pane_id, |p| p.dead).expect("pane kept by remain-on-exit");
    assert!(pane.dead, "{pane:?}");
    // …while the other session never received the remain-on-exit hook.
    let hooks = std::process::Command::new("tmux").args(["-L", &srv.socket, "show-hooks", "-t", "=other:"]).output().unwrap();
    let hooks = String::from_utf8_lossy(&hooks.stdout);
    assert!(!hooks.contains("remain-on-exit"), "hook leaked into the other session: {hooks}");
}
