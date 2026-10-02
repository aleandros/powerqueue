//! End-to-end: the real daemon drives a fake `claude` (tests/fixtures/fake-claude.sh)
//! inside a private tmux server, through real worktrees, hooks and transcripts.
//!
//! Skipped when `tmux`, `git` or `python3` are missing (the fixture needs python3
//! to parse the hook settings file).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;

struct Env {
    root: tempfile::TempDir,
    socket: String,
    daemon: Option<Child>,
}

impl Env {
    fn new(mode: &str, extra_scheduler: &str) -> Option<Self> {
        for tool in ["tmux", "git", "python3"] {
            if which::which(tool).is_err() {
                eprintln!("skipping e2e test: {tool} not on PATH");
                return None;
            }
        }
        let root = tempfile::tempdir().unwrap();
        let socket = format!("pq-e2e-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);

        let home = root.path().join("home");
        std::fs::create_dir_all(home.join("config")).unwrap();
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude.sh");
        let config = format!(
            r#"[repo]
path = "{repo}"
[linear]
enabled = false
[claude]
binary = "{fake}"
[claude.env]
POWERQUEUE_BIN = "{bin}"
FAKE_CLAUDE_MODE = "{mode}"
FAKE_CLAUDE_STATE_DIR = "{state}"
CLAUDE_CONFIG_DIR = "{claude_home}"
[tmux]
socket_name = "{socket}"
[scheduler]
tick_secs = 1
restart_backoff_secs = [1]
{extra_scheduler}
[cleanup]
push_branch = false
"#,
            repo = repo.display(),
            fake = fake.display(),
            bin = cargo_bin("powerqueue").display(),
            state = root.path().join("fakestate").display(),
            claude_home = root.path().join("claude").display(),
        );
        std::fs::write(home.join("config/config.toml"), config).unwrap();
        std::fs::write(home.join("config/PRIORITY.md"), powerqueue::priority::template()).unwrap();
        Some(Self { root, socket, daemon: None })
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(cargo_bin("powerqueue"));
        c.env("POWERQUEUE_HOME", self.home())
            .env("POWERQUEUE_SECRETS", "file")
            .env("CLAUDE_CONFIG_DIR", self.root.path().join("claude"))
            .env("NO_COLOR", "1")
            .env_remove("TMUX");
        c
    }

    fn run_ok(&self, args: &[&str]) -> String {
        let out = self.cmd().args(args).output().unwrap();
        assert!(out.status.success(), "powerqueue {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn start_daemon(&mut self) {
        let log = std::fs::File::create(self.root.path().join("daemon.log")).unwrap();
        let child = self.cmd().arg("run").stdout(Stdio::from(log.try_clone().unwrap())).stderr(Stdio::from(log)).spawn().unwrap();
        self.daemon = Some(child);
    }

    fn task_state(&self, key: &str) -> String {
        let out = self.run_ok(&["status", "--all", "--json"]);
        let v: serde_json::Value = serde_json::from_str(&out).expect("status json");
        let tasks = v.get("tasks").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        tasks.iter().find(|t| t["key"] == key).and_then(|t| t["state"].as_str()).unwrap_or("?").to_string()
    }

    fn wait_for_state(&self, key: &str, want: &str, timeout: Duration) -> String {
        let start = Instant::now();
        loop {
            let st = self.task_state(key);
            if st == want || start.elapsed() > timeout {
                return st;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Poll `task show --json` until `ready` holds or 15 s pass; returns the last snapshot.
    fn wait_for_show(&self, key: &str, ready: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let start = Instant::now();
        loop {
            let show = self.run_ok(&["--json", "task", "show", key]);
            let v: serde_json::Value = serde_json::from_str(&show).expect("task show json");
            if ready(&v) || start.elapsed() > Duration::from_secs(15) {
                return v;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.root.path().join("daemon.log")).unwrap_or_default()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.cmd().arg("stop").output();
        if let Some(mut child) = self.daemon.take() {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = Command::new("tmux").args(["-L", &self.socket, "kill-server"]).output();
    }
}

fn has_event(show: &serde_json::Value, kind: &str) -> bool {
    show["events"].as_array().map(|evs| evs.iter().any(|e| e["kind"] == kind)).unwrap_or(false)
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(repo).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn add_task(env: &Env, title: &str, extra: &[&str]) -> String {
    let mut args = vec!["--json", "add", title];
    args.extend_from_slice(extra);
    let out = env.run_ok(&args);
    let v: serde_json::Value = serde_json::from_str(&out).expect("add json");
    v["key"].as_str().expect("key").to_string()
}

#[test]
fn task_runs_to_completion_with_usage_and_cleanup() {
    let Some(mut env) = Env::new("complete", "") else { return };
    let key = add_task(&env, "Say hello", &[]);
    env.start_daemon();
    let st = env.wait_for_state(&key, "completed", Duration::from_secs(60));
    assert_eq!(st, "completed", "daemon log:\n{}", env.daemon_log());

    // `task complete` flips the state from inside the session; the daemon sweeps
    // the transcript (and runs cleanup) on its following ticks, so poll briefly.
    let v = env.wait_for_show(&key, |v| v["usage"]["output_tokens"].as_u64().unwrap_or(0) > 0 && has_event(v, "cleanup.done"));
    assert_eq!(v["task"]["summary"].as_str(), Some("fake-claude finished"));
    assert_eq!(v["task"]["attempts"].as_u64(), Some(1));
    let usage = &v["usage"];
    assert!(usage["output_tokens"].as_u64().unwrap_or(0) >= 1400, "usage from transcript was recorded: {usage}");
    let kinds: Vec<&str> = v["events"].as_array().unwrap().iter().filter_map(|e| e["kind"].as_str()).collect();
    for expected in ["task.starting", "worktree.ready", "session.launched", "session.started", "task.completed", "cleanup.done"] {
        assert!(kinds.contains(&expected), "missing event {expected} in {kinds:?}");
    }
    // The branch exists in the repo and holds the fake commit; the worktree is kept because nothing was pushed.
    let repo = env.root.path().join("repo");
    let branches = Command::new("git").args(["branch", "--list", &format!("pq/{key}")]).current_dir(&repo).output().unwrap();
    assert!(String::from_utf8_lossy(&branches.stdout).contains(&format!("pq/{key}")));
    assert!(kinds.contains(&"cleanup.kept"));
}

#[test]
fn crashed_session_is_resumed_and_completes() {
    let Some(mut env) = Env::new("crash-once", "") else { return };
    let key = add_task(&env, "Crashy", &["--label", "customer"]);
    env.start_daemon();
    let st = env.wait_for_state(&key, "completed", Duration::from_secs(90));
    assert_eq!(st, "completed", "daemon log:\n{}", env.daemon_log());
    let v = env.wait_for_show(&key, |v| has_event(v, "cleanup.done"));
    assert_eq!(v["task"]["attempts"].as_u64(), Some(2));
    assert_eq!(v["task"]["criticality"].as_str(), Some("high"), "label customer => high per PRIORITY.md");
    let kinds: Vec<&str> = v["events"].as_array().unwrap().iter().filter_map(|e| e["kind"].as_str()).collect();
    assert!(kinds.contains(&"session.crashed"), "{kinds:?}");
    let launched: Vec<&str> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "session.launched")
        .filter_map(|e| e["message"].as_str())
        .collect();
    assert_eq!(launched.len(), 2, "{launched:?}");
    assert!(launched[1].contains("resumed"), "second launch resumes the same Claude session: {launched:?}");
    let sessions = v["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "a resumed attempt reuses the session row");
}

#[test]
fn blocked_session_needs_attention() {
    let Some(mut env) = Env::new("idle", "idle_timeout_secs = 2") else { return };
    let key = add_task(&env, "Asks a question", &[]);
    env.start_daemon();
    let st = env.wait_for_state(&key, "needs_attention", Duration::from_secs(60));
    assert_eq!(st, "needs_attention", "daemon log:\n{}", env.daemon_log());
    // The session is still alive in tmux, so a human can attach and answer.
    let out = env.run_ok(&["task", "output", &key, "-n", "20"]);
    assert!(out.contains("fake-claude"), "{out}");
    let print = env.run_ok(&["attach", &key, "--print"]);
    assert!(print.contains("tmux"), "{print}");
}
