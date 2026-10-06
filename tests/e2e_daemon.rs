//! End-to-end: the real daemon drives a fake `claude` (tests/fixtures/fake-claude.sh),
//! `codex` (fake-codex.sh) or `agy` (fake-agy.sh) inside a private tmux server,
//! through real worktrees, hooks, notify commands and transcripts.
//!
//! Skipped when `tmux`, `git` or `python3` are missing (the fixtures need python3).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;

struct Env {
    root: tempfile::TempDir,
    socket: String,
    daemon: Option<Child>,
    /// Extra environment for every `powerqueue` command (and so the daemon).
    vars: Vec<(String, String)>,
}

impl Env {
    fn new(mode: &str, extra_scheduler: &str) -> Option<Self> {
        Self::with_provider(mode, extra_scheduler, |_| String::new())
    }

    /// Claude disabled, Codex enabled and launched through fake-codex.sh.
    fn codex(mode: &str) -> Option<Self> {
        let mut env = Self::with_provider("complete", "", |root| {
            let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-codex.sh");
            format!(
                r#"[budget]
default_model = "gpt-6-astra"
low_model = "gpt-6-luna"
[budget.providers.claude]
enabled = false
[budget.providers.codex]
enabled = true
# Exercise the provider independently of weekly criticality relaxation.
[budget.providers.codex.models.gpt-6-astra]
min_criticality = "normal"
[codex]
binary = "{fake}"
[codex.env]
POWERQUEUE_BIN = "{bin}"
FAKE_CODEX_MODE = "{mode}"
FAKE_CODEX_STATE_DIR = "{state}"
CODEX_HOME = "{codex_home}"
"#,
                fake = fake.display(),
                bin = cargo_bin("powerqueue").display(),
                state = root.join("fakestate").display(),
                codex_home = root.join("codex").display(),
            )
        })?;
        let home = env.root.path().join("codex").display().to_string();
        env.vars.push(("CODEX_HOME".into(), home));
        Some(env)
    }

    /// Claude disabled, Antigravity enabled and launched through fake-agy.sh.
    fn gemini(mode: &str) -> Option<Self> {
        let mut env = Self::with_provider("complete", "", |root| {
            let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-agy.sh");
            format!(
                r#"[budget]
default_model = "gemini-3-pro"
low_model = "gemini-3-flash"
[budget.providers.claude]
enabled = false
[budget.providers.gemini]
enabled = true
# Exercise the provider independently of weekly criticality relaxation.
[budget.providers.gemini.models.gemini-3-pro]
min_criticality = "normal"
[gemini]
binary = "{fake}"
[gemini.env]
POWERQUEUE_BIN = "{bin}"
FAKE_AGY_MODE = "{mode}"
POWERQUEUE_AGY_HOME = "{agy_home}"
"#,
                fake = fake.display(),
                bin = cargo_bin("powerqueue").display(),
                agy_home = root.join("agy").display(),
            )
        })?;
        let home = env.root.path().join("agy").display().to_string();
        env.vars.push(("POWERQUEUE_AGY_HOME".into(), home));
        Some(env)
    }

    fn with_provider(mode: &str, extra_scheduler: &str, provider_config: impl Fn(&Path) -> String) -> Option<Self> {
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
{provider_config}
"#,
            provider_config = provider_config(root.path()),
            repo = repo.display(),
            fake = fake.display(),
            bin = cargo_bin("powerqueue").display(),
            state = root.path().join("fakestate").display(),
            claude_home = root.path().join("claude").display(),
        );
        std::fs::write(home.join("config/config.toml"), config).unwrap();
        std::fs::write(home.join("config/PRIORITY.md"), powerqueue::priority::template()).unwrap();
        Some(Self { root, socket, daemon: None, vars: Vec::new() })
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
            .env_remove("TMUX")
            .envs(self.vars.iter().map(|(k, v)| (k, v)));
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

    /// Poll `task show --json` until `ready` holds or 30 s pass (generous for
    /// loaded CI runners; a passing check returns at once); returns the last
    /// snapshot.
    fn wait_for_show(&self, key: &str, ready: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let start = Instant::now();
        loop {
            let show = self.run_ok(&["--json", "task", "show", key]);
            let v: serde_json::Value = serde_json::from_str(&show).expect("task show json");
            if ready(&v) || start.elapsed() > Duration::from_secs(30) {
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

/// A task can be completed by either signal, whichever the daemon sees
/// first: the agent's `powerqueue task complete` (`task.completed_by_command`)
/// or the done marker / Stop hook (`task.completed`).
fn completed_event(kinds: &[impl AsRef<str>]) -> bool {
    kinds.iter().any(|k| matches!(k.as_ref(), "task.completed" | "task.completed_by_command"))
}

/// Output tokens recorded so far.
fn output_tokens(show: &serde_json::Value) -> u64 {
    show["usage"]["output_tokens"].as_u64().unwrap_or(0)
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(repo).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Wait until the daemon starts `key` or throttles it first. The budget
/// policy may not schedule other providers' models yet (that lands with the
/// multi-provider policy); then the test is skipped with a note.
fn launched_or_skip(env: &Env, key: &str) -> bool {
    let v = env.wait_for_show(key, |v| has_event(v, "task.starting") || has_event(v, "task.throttled"));
    if has_event(&v, "task.starting") {
        return true;
    }
    assert!(has_event(&v, "task.throttled"), "the daemon neither started nor throttled {key}; log:\n{}", env.daemon_log());
    let reasons: Vec<String> = v["events"]
        .as_array()
        .map(|evs| evs.iter().filter(|e| e["kind"] == "task.throttled").map(|e| e["data"]["reasons"].to_string()).collect())
        .unwrap_or_default();
    eprintln!("skipping: the budget policy did not schedule {key} on its non-Claude model: {reasons:?}");
    false
}

fn event_kinds(show: &serde_json::Value) -> Vec<String> {
    show["events"]
        .as_array()
        .map(|evs| evs.iter().filter_map(|e| e["kind"].as_str().map(String::from)).collect())
        .unwrap_or_default()
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
    // the transcript (and runs cleanup) on its following ticks, possibly one
    // turn at a time, so wait for the final totals rather than the first ones.
    let v = env.wait_for_show(&key, |v| output_tokens(v) >= 1400 && has_event(v, "cleanup.done"));
    assert_eq!(v["task"]["summary"].as_str(), Some("fake-claude finished"));
    assert_eq!(v["task"]["attempts"].as_u64(), Some(1));
    let usage = &v["usage"];
    assert!(usage["output_tokens"].as_u64().unwrap_or(0) >= 1400, "usage from transcript was recorded: {usage}");
    let kinds: Vec<&str> = v["events"].as_array().unwrap().iter().filter_map(|e| e["kind"].as_str()).collect();
    for expected in ["task.starting", "worktree.ready", "session.launched", "session.started", "cleanup.done"] {
        assert!(kinds.contains(&expected), "missing event {expected} in {kinds:?}");
    }
    assert!(completed_event(&kinds), "no completion event in {kinds:?}");
    // The branch exists in the repo and holds the fake commit; the worktree is kept because nothing was pushed.
    let repo = env.root.path().join("repo");
    let branches = Command::new("git").args(["branch", "--list", &format!("pq/{key}")]).current_dir(&repo).output().unwrap();
    assert!(String::from_utf8_lossy(&branches.stdout).contains(&format!("pq/{key}")));
    assert!(kinds.contains(&"cleanup.kept"));
}

/// A commit merged on the remote (a blocker's PR) is in the next task's
/// branch even though the local `main` is behind; the base is recorded and
/// the clean main checkout is fast-forwarded.
#[test]
fn new_task_branch_starts_from_fetched_origin() {
    let Some(mut env) = Env::new("complete", "") else { return };
    let repo = env.root.path().join("repo");
    let bare = env.root.path().join("origin.git");
    git(env.root.path(), &["init", "-q", "--bare", "-b", "main", &bare.to_string_lossy()]);
    git(&repo, &["remote", "add", "origin", &bare.to_string_lossy()]);
    git(&repo, &["push", "-q", "origin", "main"]);
    let other = env.root.path().join("other");
    git(env.root.path(), &["clone", "-q", &bare.to_string_lossy(), &other.to_string_lossy()]);
    std::fs::write(other.join("blocker.txt"), "merged\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "blocker merged"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let merged =
        String::from_utf8_lossy(&Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&other).output().unwrap().stdout)
            .trim()
            .to_string();

    let key = add_task(&env, "Build on the blocker", &[]);
    env.start_daemon();
    let v = env.wait_for_show(&key, |v| has_event(v, "worktree.ready"));
    assert_eq!(v["base"]["base"].as_str(), Some("origin/main"), "show: {v}\nlog:\n{}", env.daemon_log());
    assert_eq!(v["base"]["base_sha"].as_str(), Some(merged.as_str()));
    let contains = Command::new("git")
        .args(["merge-base", "--is-ancestor", &merged, &format!("pq/{key}")])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(contains.success(), "pq/{key} holds the merged commit");
    assert!(repo.join("blocker.txt").exists(), "the clean main checkout was fast-forwarded");
    let text = env.run_ok(&["task", "show", &key]);
    assert!(text.contains(&format!("origin/main at {merged}")), "{text}");
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

#[test]
fn terminal_reply_clears_attention_in_status_and_dashboard() {
    let Some(mut env) = Env::new("attention-reply", "") else { return };
    let key = add_task(&env, "Wait for my answer", &[]);
    env.start_daemon();
    assert_eq!(env.wait_for_state(&key, "needs_attention", Duration::from_secs(60)), "needs_attention");
    env.run_ok(&["task", "send", &key, "Continue"]);
    assert_eq!(env.wait_for_state(&key, "running", Duration::from_secs(30)), "running", "{}", env.daemon_log());
    let show = env.wait_for_show(&key, |v| has_event(v, "task.attention_resolved"));
    assert!(show["task"]["last_error"].is_null());
    let dashboard: serde_json::Value = serde_json::from_str(&env.run_ok(&["dashboard", "--once", "--json"])).unwrap();
    let task = dashboard["tasks"].as_array().unwrap().iter().find(|t| t["key"] == key).unwrap();
    assert_eq!(task["state"], "running");
}

#[test]
fn task_runs_on_codex_when_selected() {
    let Some(mut env) = Env::codex("complete") else { return };
    let key = add_task(&env, "Say hello on codex", &["--model", "gpt-6-astra"]);
    env.start_daemon();
    if !launched_or_skip(&env, &key) {
        return;
    }
    let st = env.wait_for_state(&key, "completed", Duration::from_secs(60));
    assert_eq!(st, "completed", "daemon log:\n{}", env.daemon_log());
    // The notify payload (a Stop hook) can be drained after cleanup; wait for it too.
    let v = env.wait_for_show(&key, |v| output_tokens(v) >= 1400 && has_event(v, "cleanup.done") && has_event(v, "hook.stop"));
    let session = &v["sessions"][0]["session"];
    assert_eq!(session["model"].as_str(), Some("gpt-6-astra"), "{session}");
    assert!(session["agent_session_id"].as_str().is_some_and(|s| !s.is_empty()), "thread id discovered: {session}");
    assert!(session["transcript_path"].as_str().unwrap_or("").contains("rollout-"), "{session}");
    assert_eq!(v["task"]["summary"].as_str(), Some("fake-codex finished"));
    let usage = &v["usage"];
    assert_eq!(usage["output_tokens"].as_u64(), Some(1400), "{usage}");
    assert_eq!(usage["cache_read_input_tokens"].as_u64(), Some(12032 + 15000), "{usage}");
    assert_eq!(usage["input_tokens"].as_u64(), Some(18699 - 12032 + 20000 - 15000), "{usage}");
    let kinds = event_kinds(&v);
    for expected in ["task.starting", "session.launched", "session.discovered", "cleanup.done"] {
        assert!(kinds.iter().any(|k| k == expected), "missing event {expected} in {kinds:?}");
    }
    assert!(completed_event(&kinds), "no completion event in {kinds:?}");
    // The notify payload reached `powerqueue hook --provider codex` as a Stop.
    assert!(kinds.iter().any(|k| k == "hook.stop"), "{kinds:?}");
}

#[test]
fn codex_crash_resumes_the_same_thread() {
    let Some(mut env) = Env::codex("crash") else { return };
    let key = add_task(&env, "Crashy codex", &["--model", "gpt-6-astra"]);
    env.start_daemon();
    if !launched_or_skip(&env, &key) {
        return;
    }
    let st = env.wait_for_state(&key, "completed", Duration::from_secs(90));
    assert_eq!(st, "completed", "daemon log:\n{}", env.daemon_log());
    let v = env.wait_for_show(&key, |v| has_event(v, "cleanup.done"));
    assert_eq!(v["task"]["attempts"].as_u64(), Some(2));
    let launched: Vec<&str> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "session.launched")
        .filter_map(|e| e["message"].as_str())
        .collect();
    assert_eq!(launched.len(), 2, "{launched:?}");
    assert!(launched[1].contains("resumed"), "the second launch resumes the Codex thread: {launched:?}");
    let sessions = v["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "a resumed attempt reuses the session row");
    assert!(sessions[0]["session"]["agent_session_id"].as_str().is_some());
    // Both runs appended to one rollout.
    let rollouts: Vec<_> = walk(&env.root.path().join("codex/sessions"));
    assert_eq!(rollouts.len(), 1, "{rollouts:?}\ndaemon log:\n{}", env.daemon_log());
}

#[test]
fn codex_rate_limit_puts_provider_on_cooldown() {
    let Some(mut env) = Env::codex("ratelimit") else { return };
    let key = add_task(&env, "Throttled codex", &["--model", "gpt-6-astra"]);
    env.start_daemon();
    if !launched_or_skip(&env, &key) {
        return;
    }
    let st = env.wait_for_state(&key, "throttled", Duration::from_secs(60));
    assert_eq!(st, "throttled", "daemon log:\n{}", env.daemon_log());
    let v = env.wait_for_show(&key, |v| has_event(v, "budget.rate_limited"));
    let kinds = event_kinds(&v);
    assert!(kinds.iter().any(|k| k == "session.rate_limit_detected"), "{kinds:?}");
    let limited = v["events"].as_array().unwrap().iter().find(|e| e["kind"] == "budget.rate_limited").expect("rate limit event");
    assert!(limited["message"].as_str().unwrap_or("").contains("gpt-6-astra"), "names the codex model: {limited}");
    assert_eq!(limited["data"]["error_type"].as_str(), Some("rate_limit"));
    assert!(v["task"]["last_error"].as_str().unwrap_or("").contains("usage limit"), "{}", v["task"]);
}

#[test]
fn task_completes_on_gemini_by_transcript_polling() {
    let Some(mut env) = Env::gemini("poll-only") else { return };
    let key = add_task(&env, "Say hello on agy", &["--model", "gemini-3-pro"]);
    env.start_daemon();
    if !launched_or_skip(&env, &key) {
        return;
    }
    let st = env.wait_for_state(&key, "completed", Duration::from_secs(60));
    assert_eq!(st, "completed", "daemon log:\n{}", env.daemon_log());
    let v = env.wait_for_show(&key, |v| has_event(v, "cleanup.done"));
    assert_eq!(v["task"]["summary"].as_str(), Some("fake-agy finished"));
    let session = &v["sessions"][0]["session"];
    assert_eq!(session["model"].as_str(), Some("gemini-3-pro"));
    assert!(session["agent_session_id"].as_str().is_some(), "conversation discovered: {session}");
    let kinds = event_kinds(&v);
    for expected in ["session.discovered", "session.marker_polled", "task.completed", "cleanup.done"] {
        assert!(kinds.iter().any(|k| k == expected), "missing event {expected} in {kinds:?}");
    }
    // The hook file was written into the worktree and kept out of git.
    let exclude = std::fs::read_to_string(env.root.path().join("repo/.git/info/exclude")).unwrap_or_default();
    assert!(exclude.lines().any(|l| l == ".agents/"), "{exclude}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// GraphQL response of the fake `gh` for PR `number`.
fn gh_pr(number: u64, state: &str, mergeable: &str) -> String {
    serde_json::json!({ "data": { "repository": { "pullRequest": {
        "number": number, "state": state, "mergeable": mergeable, "mergeStateStatus": "CLEAN", "headRefOid": "abc1234",
        "autoMergeRequest": { "enabledAt": "2026-10-06T10:00:00Z" },
        "labels": { "nodes": [] },
        "reviewThreads": { "nodes": [] },
        "commits": { "nodes": [] }
    } } } })
    .to_string()
}

#[test]
fn review_hand_off_frees_the_slot_and_a_conflict_resumes_the_same_session() {
    let gh = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-gh.sh");
    let extra = format!("max_concurrent = 1\npr_poll_secs = 1\ngh_binary = \"{}\"", gh.display());
    let Some(mut env) = Env::new("pr", &extra) else { return };
    let state = env.root.path().join("fakestate");
    std::fs::create_dir_all(&state).unwrap();
    env.vars.push(("FAKE_GH_DIR".into(), state.display().to_string()));
    std::fs::write(state.join("pr-7.json"), gh_pr(7, "OPEN", "MERGEABLE")).unwrap();
    std::fs::write(state.join("pr-8.json"), gh_pr(8, "OPEN", "MERGEABLE")).unwrap();
    let a = add_task(&env, "Ship A", &[]);
    std::fs::write(state.join(format!("pr-{a}")), "7").unwrap();
    let b = add_task(&env, "Ship B", &[]);
    std::fs::write(state.join(format!("pr-{b}")), "8").unwrap();
    env.start_daemon();

    assert_eq!(env.wait_for_state(&a, "in_review", Duration::from_secs(60)), "in_review", "{}", env.daemon_log());
    // max_concurrent = 1: B can only run because A in review holds no slot.
    assert_eq!(env.wait_for_state(&b, "in_review", Duration::from_secs(60)), "in_review", "{}", env.daemon_log());
    let v = env.wait_for_show(&a, |v| has_event(v, "session.released") && has_event(v, "review.status"));
    assert_eq!(v["task"]["pr_url"].as_str(), Some("https://github.com/o/r/pull/7"));
    assert!(v["sessions"][0]["session"]["state"] == "exited", "{}", v["sessions"]);

    // A's PR now conflicts. The resumed session "resolves" it (the next gh
    // answer is MERGED), re-arms, and the watcher completes the task.
    std::fs::write(state.join("pr-7.next.json"), gh_pr(7, "MERGED", "UNKNOWN")).unwrap();
    std::fs::write(state.join("pr-7.json"), gh_pr(7, "OPEN", "CONFLICTING")).unwrap();
    let v = env.wait_for_show(&a, |v| v["task"]["state"] == "completed" && has_event(v, "cleanup.branch_deleted"));
    assert_eq!(v["task"]["state"].as_str(), Some("completed"), "{}", env.daemon_log());
    let kinds = event_kinds(&v);
    for expected in ["task.in_review", "review.relaunch", "review.merged", "cleanup.branch_deleted"] {
        assert!(kinds.iter().any(|k| k == expected), "missing event {expected} in {kinds:?}");
    }
    let sessions = v["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "the review round resumed the original session row");
    let session_id = sessions[0]["session"]["id"].as_str().unwrap().to_string();
    let prompt = std::fs::read_to_string(state.join(format!("resume-prompt-{session_id}.txt"))).unwrap_or_default();
    assert_eq!(prompt.trim(), "/ship-pr 7 --reason conflict", "resumed with the review prompt");
    let launched: Vec<&str> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "session.launched")
        .filter_map(|e| e["message"].as_str())
        .collect();
    assert!(launched.last().is_some_and(|m| m.contains("resumed") && m.contains("review: conflict")), "{launched:?}");
    assert_eq!(v["task"]["review"]["rounds"].as_u64(), Some(1));
    let branches = Command::new("git")
        .args(["branch", "--list", &format!("pq/{a}")])
        .current_dir(env.root.path().join("repo"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&branches.stdout).trim().is_empty(), "local branch deleted after the merge");
    assert_eq!(env.task_state(&b), "in_review", "B's PR is still open");
}
