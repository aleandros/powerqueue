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
        Self::with_opts(Opts { mode, extra_scheduler, ..Opts::default() }, provider_config)
    }

    /// The general form; see [`Opts`].
    fn with_opts(opts: Opts<'_>, provider_config: impl Fn(&Path) -> String) -> Option<Self> {
        for tool in ["tmux", "git", "python3"] {
            if which::which(tool).is_err() {
                eprintln!("skipping e2e test: {tool} not on PATH");
                return None;
            }
        }
        // A physical path: the container tests mount the root at the same
        // path inside the container, where macOS's `/var` → `/private/var`
        // symlink does not exist, and Claude Code derives the transcript
        // path from the physical working directory.
        let temp_base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = tempfile::tempdir_in(temp_base).unwrap();
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
        let claude_binary = opts.claude_binary.clone().unwrap_or_else(|| fake.display().to_string());
        let powerqueue_bin = if opts.launcher_sets_powerqueue_bin {
            String::new()
        } else {
            format!("POWERQUEUE_BIN = \"{}\"", cargo_bin("powerqueue").display())
        };
        let config = format!(
            r#"[repo]
path = "{repo}"
{extra_repo}
[linear]
enabled = false
[claude]
binary = "{claude_binary}"
{extra_claude}
[claude.env]
{powerqueue_bin}
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
{extra_cleanup}
{provider_config}
"#,
            provider_config = provider_config(root.path()),
            repo = repo.display(),
            extra_repo = opts.extra_repo,
            extra_claude = opts.extra_claude,
            extra_cleanup = opts.extra_cleanup,
            mode = opts.mode,
            extra_scheduler = opts.extra_scheduler,
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
            .env_remove("GITHUB_TOKEN")
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

/// How [`Env::with_opts`] writes `config.toml`.
#[derive(Default)]
struct Opts<'a> {
    /// `FAKE_CLAUDE_MODE`.
    mode: &'a str,
    /// Extra lines in `[scheduler]`.
    extra_scheduler: &'a str,
    /// `[claude] binary`; the fixture when `None`.
    claude_binary: Option<String>,
    /// Extra lines in `[claude]` (e.g. `shim = true`).
    extra_claude: &'a str,
    /// Leave `POWERQUEUE_BIN` to the launcher (the shim, or the binary)
    /// instead of pointing the fixture at this build's binary.
    launcher_sets_powerqueue_bin: bool,
    /// Extra lines in `[repo]` (e.g. `setup = [...]`).
    extra_repo: String,
    /// Extra lines in `[cleanup]` (e.g. `run = [...]`).
    extra_cleanup: String,
}

/// Where the generated shim and inbox of task `key` live under the test home.
fn shim_and_inbox(env: &Env, key: &str) -> (PathBuf, PathBuf) {
    let show = env.run_ok(&["--json", "task", "show", key]);
    let v: serde_json::Value = serde_json::from_str(&show).expect("task show json");
    let id = v["task"]["id"].as_str().expect("task id");
    let task_dir = env.home().join("state/tasks").join(id);
    (task_dir.join("bin/powerqueue"), task_dir.join("inbox"))
}

/// The fixture finds the generated shim on PATH (`POWERQUEUE_BIN` is the
/// shim), its hooks call the shim by absolute path, and everything reaches
/// the daemon through `<task dir>/inbox` instead of the binary.
#[test]
fn shim_routes_hooks_and_completion_through_the_inbox() {
    let opts = Opts { mode: "complete", extra_claude: "shim = true", launcher_sets_powerqueue_bin: true, ..Opts::default() };
    let Some(mut env) = Env::with_opts(opts, |_| String::new()) else { return };
    let key = add_task(&env, "Say hello through the shim", &[]);
    env.start_daemon();
    let st = env.wait_for_state(&key, "completed", Duration::from_secs(60));
    assert_eq!(st, "completed", "daemon log:\n{}", env.daemon_log());
    let v = env.wait_for_show(&key, |v| output_tokens(v) >= 1400 && has_event(v, "cleanup.done"));
    assert_eq!(v["task"]["summary"].as_str(), Some("fake-claude finished"));
    let kinds = event_kinds(&v);
    // `task complete` arrived as an inbox message, the hooks as hook rows.
    for expected in ["inbox.complete", "hook.sessionstart", "hook.stop", "session.started", "cleanup.done"] {
        assert!(kinds.iter().any(|k| k == expected), "missing event {expected} in {kinds:?}");
    }
    assert!(!kinds.iter().any(|k| k == "inbox.rejected" || k == "inbox.error"), "{kinds:?}");
    assert!(output_tokens(&v) >= 1400, "usage came from the transcript: {}", v["usage"]);
    let (shim, inbox) = shim_and_inbox(&env, &key);
    assert!(shim.is_file(), "shim at {}", shim.display());
    let left: Vec<PathBuf> = walk(&inbox).into_iter().filter(|p| p.extension().is_some_and(|e| e == "msg")).collect();
    assert!(left.is_empty(), "the inbox was drained: {left:?}");
    assert!(!inbox.join("rejected").exists(), "nothing was rejected");
    let settings = std::fs::read_to_string(shim.parent().unwrap().parent().unwrap().join("settings.json")).unwrap();
    assert!(settings.contains(&format!("{} hook --task", shim.display())), "{settings}");
}

/// Name of the image the container tests use; built from the fixtures when
/// `POWERQUEUE_E2E_DOCKER` is set and `docker` works, else `None` (skip).
fn docker_image() -> Option<String> {
    if std::env::var_os("POWERQUEUE_E2E_DOCKER").is_none() {
        eprintln!("skipping container test: set POWERQUEUE_E2E_DOCKER=1 to run it");
        return None;
    }
    let ok = Command::new("docker").arg("info").stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success());
    if !matches!(ok, Ok(true)) {
        eprintln!("skipping container test: docker is not available");
        return None;
    }
    let image = "powerqueue-e2e-fakes".to_string();
    let ctx = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for f in ["fake-claude.sh", "fake-codex.sh"] {
        std::fs::copy(fixtures.join(f), ctx.path().join(f)).unwrap();
    }
    std::fs::write(
        ctx.path().join("Dockerfile"),
        "FROM alpine:3.20\n\
         RUN apk add --no-cache bash git python3 coreutils\n\
         COPY fake-claude.sh /usr/local/bin/fake-claude\n\
         COPY fake-codex.sh /usr/local/bin/fake-codex\n\
         RUN chmod +x /usr/local/bin/fake-claude /usr/local/bin/fake-codex\n",
    )
    .unwrap();
    let out = Command::new("docker").args(["build", "-q", "-t", &image]).arg(ctx.path()).output().unwrap();
    assert!(out.status.success(), "docker build failed: {}", String::from_utf8_lossy(&out.stderr));
    Some(image)
}

/// `repo.setup` / `cleanup.run` lines that start and remove a per-task
/// container with the whole test root mounted at the same path (worktree,
/// main checkout, state dir, the CLI's config dir and the fixture state).
fn container_setup_and_cleanup(image: &str) -> (String, String) {
    let setup = format!(
        "setup = ['''root=$(dirname \"$(dirname \"$POWERQUEUE_STATE_DIR\")\"); \
         docker rm -f pq-$POWERQUEUE_TASK_SLUG >/dev/null 2>&1; \
         docker run -d --name pq-$POWERQUEUE_TASK_SLUG -v \"$root:$root\" \
         -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0='*' \
         {image} sleep infinity >/dev/null''']"
    );
    let cleanup = "run = ['''docker rm -f pq-$POWERQUEUE_TASK_SLUG >/dev/null''']".to_string();
    (setup, cleanup)
}

/// Removes the task's container even when a test fails half-way.
struct ContainerGuard(String);

impl ContainerGuard {
    fn running(&self) -> bool {
        let out = Command::new("docker").args(["ps", "-q", "--filter", &format!("name=^{}$", self.0)]).output().unwrap();
        !String::from_utf8_lossy(&out.stdout).trim().is_empty()
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

/// A Claude session runs inside a container: `claude.binary` is a
/// `docker exec` template, the shim carries hooks and `task complete` out,
/// the transcript is read from the mounted config dir, and cleanup removes
/// the container.
#[test]
fn container_claude_session_completes_through_docker_exec() {
    let Some(image) = docker_image() else { return };
    let (setup, cleanup) = container_setup_and_cleanup(&image);
    let opts = Opts {
        mode: "complete",
        claude_binary: Some("docker exec -i -t --env-file {task_dir}/env -w {worktree} pq-{slug} fake-claude".into()),
        extra_claude: "shim = true",
        launcher_sets_powerqueue_bin: true,
        extra_repo: setup,
        extra_cleanup: cleanup,
        ..Opts::default()
    };
    let Some(mut env) = Env::with_opts(opts, |_| String::new()) else { return };
    let key = add_task(&env, "Say hello from a container", &[]);
    let guard = ContainerGuard(format!("pq-{}", key.to_lowercase()));
    env.start_daemon();
    let st = env.wait_for_state(&key, "completed", Duration::from_secs(120));
    assert_eq!(st, "completed", "daemon log:\n{}", env.daemon_log());
    let v = env.wait_for_show(&key, |v| output_tokens(v) >= 1400 && has_event(v, "cleanup.done"));
    assert_eq!(v["task"]["summary"].as_str(), Some("fake-claude finished"));
    let kinds = event_kinds(&v);
    for expected in [
        "worktree.setup",
        "session.launched",
        "session.started",
        "inbox.complete",
        "hook.stop",
        "cleanup.commands",
        "cleanup.done",
    ] {
        assert!(kinds.iter().any(|k| k == expected), "missing event {expected} in {kinds:?}");
    }
    assert!(!kinds.iter().any(|k| k == "inbox.rejected" || k == "inbox.error"), "{kinds:?}");
    assert!(output_tokens(&v) >= 1400, "usage from the transcript written inside the container: {}", v["usage"]);
    // The fixture committed inside the container, on the host's worktree.
    let repo = env.root.path().join("repo");
    let log = Command::new("git").args(["log", "--oneline", &format!("pq/{key}")]).current_dir(&repo).output().unwrap();
    assert!(
        String::from_utf8_lossy(&log.stdout).contains("fake-claude: work on task"),
        "{}",
        String::from_utf8_lossy(&log.stdout)
    );
    assert!(!guard.running(), "cleanup.run removed the container");
    let (shim, inbox) = shim_and_inbox(&env, &key);
    assert!(shim.is_file());
    assert!(walk(&inbox).into_iter().all(|p| p.extension().is_none_or(|e| e != "msg")), "inbox drained");
}

/// Same through Codex: `notify` and `task complete` go through the shim,
/// the rollout is discovered in the mounted `CODEX_HOME`.
#[test]
fn container_codex_session_completes_through_docker_exec() {
    let Some(image) = docker_image() else { return };
    let (setup, cleanup) = container_setup_and_cleanup(&image);
    let opts = Opts { mode: "complete", extra_repo: setup, extra_cleanup: cleanup, ..Opts::default() };
    let Some(mut env) = Env::with_opts(opts, |root| {
        format!(
            r#"[budget]
default_model = "gpt-6-astra"
low_model = "gpt-6-luna"
[budget.providers.claude]
enabled = false
[budget.providers.codex]
enabled = true
[budget.providers.codex.models.gpt-6-astra]
min_criticality = "normal"
[codex]
binary = "docker exec -i -t --env-file {{task_dir}}/env -w {{worktree}} pq-{{slug}} fake-codex"
shim = true
[codex.env]
FAKE_CODEX_MODE = "complete"
FAKE_CODEX_STATE_DIR = "{state}"
CODEX_HOME = "{codex_home}"
"#,
            state = root.join("fakestate").display(),
            codex_home = root.join("codex").display(),
        )
    }) else {
        return;
    };
    let codex_home = env.root.path().join("codex").display().to_string();
    env.vars.push(("CODEX_HOME".into(), codex_home));
    let key = add_task(&env, "Codex in a container", &[]);
    let guard = ContainerGuard(format!("pq-{}", key.to_lowercase()));
    env.start_daemon();
    if !launched_or_skip(&env, &key) {
        return;
    }
    let st = env.wait_for_state(&key, "completed", Duration::from_secs(120));
    assert_eq!(st, "completed", "daemon log:\n{}", env.daemon_log());
    let v = env.wait_for_show(&key, |v| has_event(v, "cleanup.done") && output_tokens(v) > 0);
    assert_eq!(v["task"]["summary"].as_str(), Some("fake-codex finished"));
    let kinds = event_kinds(&v);
    for expected in ["session.launched", "session.discovered", "inbox.complete", "hook.stop", "cleanup.commands", "cleanup.done"]
    {
        assert!(kinds.iter().any(|k| k == expected), "missing event {expected} in {kinds:?}");
    }
    assert!(!kinds.iter().any(|k| k == "inbox.rejected" || k == "inbox.error"), "{kinds:?}");
    assert!(output_tokens(&v) > 0, "usage from the rollout in the mounted CODEX_HOME: {}", v["usage"]);
    assert!(!guard.running(), "cleanup.run removed the container");
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

    // repo.setup fails on the first attempt only: the retry reuses the
    // branch, and the base recorded at creation still shows.
    let marker = env.root.path().join("setup-failed-once");
    std::fs::write(
        repo.join(".powerqueue.toml"),
        format!("setup = [\"test -f {m} || {{ touch {m}; exit 1; }}\"]\n", m = marker.display()),
    )
    .unwrap();
    let key = add_task(&env, "Build on the blocker", &[]);
    env.start_daemon();
    let v = env.wait_for_show(&key, |v| has_event(v, "worktree.ready"));
    let ready: Vec<&serde_json::Value> =
        v["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "worktree.ready").collect();
    assert_eq!(ready.len(), 1, "{ready:?}");
    assert_eq!(ready[0]["data"]["new_branch"].as_bool(), Some(false), "the retry reused the branch");
    assert_eq!(ready[0]["data"]["base_sha"].as_str(), Some(merged.as_str()));
    assert_eq!(v["base"]["ref"].as_str(), Some("origin/main"), "show: {v}\nlog:\n{}", env.daemon_log());
    assert_eq!(v["base"]["sha"].as_str(), Some(merged.as_str()));
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
fn completion_without_a_pr_link_adopts_the_branch_open_pr() {
    // AVS-1652: the agent printed the done marker without `--pr`; its
    // branch had an open PR, and the task vanished as `completed`.
    let gh = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-gh.sh");
    let extra = format!("pr_poll_secs = 1\ngh_binary = \"{}\"", gh.display());
    let Some(mut env) = Env::new("adopt", &extra) else { return };
    let state = env.root.path().join("fakestate");
    std::fs::create_dir_all(&state).unwrap();
    env.vars.push(("FAKE_GH_DIR".into(), state.display().to_string()));
    std::fs::write(state.join("pr-list.json"), r#"[{"url":"https://github.com/o/r/pull/9"}]"#).unwrap();
    std::fs::write(state.join("pr-9.json"), gh_pr(9, "OPEN", "MERGEABLE")).unwrap();
    let a = add_task(&env, "Ship without link", &[]);
    env.start_daemon();

    let v = env.wait_for_show(&a, |v| v["task"]["state"] == "in_review" && has_event(v, "session.released"));
    assert_eq!(v["task"]["state"].as_str(), Some("in_review"), "{}", env.daemon_log());
    assert_eq!(v["task"]["pr_url"].as_str(), Some("https://github.com/o/r/pull/9"));
    let calls = std::fs::read_to_string(state.join("gh-calls.log")).unwrap_or_default();
    assert!(calls.contains("pr list head=pq/"), "{calls}");
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

/// Stateful REST fixture: the daemon must observe labels changed by earlier
/// writes, while unrelated labels and non-issue entries exercise intake filtering.
#[derive(Clone)]
struct GitHubFixture(std::sync::Arc<std::sync::Mutex<GitHubFixtureState>>);

struct GitHubFixtureState {
    issue: serde_json::Value,
    comments: Vec<String>,
    operations: Vec<String>,
}

impl wiremock::Respond for GitHubFixture {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        use serde_json::json;
        use wiremock::ResponseTemplate;
        if request.headers.get("authorization").and_then(|h| h.to_str().ok()) != Some("Bearer github-e2e-token") {
            return ResponseTemplate::new(401);
        }
        let mut state = self.0.lock().unwrap();
        let method = request.method.as_str();
        let path = request.url.path();
        match (method, path) {
            ("GET", "/repos/acme/app") => ResponseTemplate::new(200).set_body_json(json!({"full_name": "acme/app"})),
            ("GET", "/repos/acme/app/issues") => {
                let mut entries = Vec::new();
                if state.issue["state"] == "open" {
                    entries.push(state.issue.clone());
                }
                let mut pr = state.issue.clone();
                pr["number"] = json!(2);
                pr["state"] = json!("open");
                pr["pull_request"] = json!({"url": "https://api.github.com/repos/acme/app/pulls/2"});
                entries.push(pr);
                let mut excluded = state.issue.clone();
                excluded["number"] = json!(3);
                excluded["state"] = json!("open");
                excluded["labels"] = json!([{"name": "powerqueue"}, {"name": "no-agent"}]);
                entries.push(excluded);
                ResponseTemplate::new(200).set_body_json(entries)
            }
            ("GET", "/repos/acme/app/issues/1") => ResponseTemplate::new(200).set_body_json(&state.issue),
            ("POST", "/repos/acme/app/issues/1/labels") => {
                let body: serde_json::Value = request.body_json().unwrap();
                for label in body["labels"].as_array().unwrap() {
                    state.operations.push(format!("label:{}", label.as_str().unwrap()));
                    let labels = state.issue["labels"].as_array_mut().unwrap();
                    if !labels.iter().any(|l| l["name"] == *label) {
                        labels.push(json!({"name": label}));
                    }
                }
                ResponseTemplate::new(200).set_body_json(&state.issue["labels"])
            }
            ("DELETE", "/repos/acme/app/issues/1/labels/agent:working") => {
                state.operations.push("remove:agent:working".into());
                state.issue["labels"].as_array_mut().unwrap().retain(|l| l["name"] != "agent:working");
                ResponseTemplate::new(200).set_body_json(&state.issue["labels"])
            }
            ("POST", "/repos/acme/app/issues/1/comments") => {
                let body: serde_json::Value = request.body_json().unwrap();
                state.comments.push(body["body"].as_str().unwrap().to_string());
                state.operations.push("comment".into());
                ResponseTemplate::new(201).set_body_json(json!({"id": state.comments.len()}))
            }
            ("PATCH", "/repos/acme/app/issues/1") => {
                let body: serde_json::Value = request.body_json().unwrap();
                if body != json!({"state": "closed", "state_reason": "completed"}) {
                    return ResponseTemplate::new(422);
                }
                state.operations.push("close".into());
                state.issue["state"] = json!("closed");
                ResponseTemplate::new(200).set_body_json(&state.issue)
            }
            _ => ResponseTemplate::new(404),
        }
    }
}

async fn github_lifecycle_e2e(close_on_complete: bool, with_review: bool) {
    use serde_json::json;
    use wiremock::{Mock, MockServer};
    // Construct Env before the server so prerequisites still skip cleanly.
    let Some(mut env) = Env::new("complete", "") else { return };
    let server = MockServer::start().await;
    let remote_state = GitHubFixture(std::sync::Arc::new(std::sync::Mutex::new(GitHubFixtureState {
        issue: json!({
            "number": 1, "title": "GitHub end-to-end task", "body": "Make the fixture commit in the dedicated worktree.",
            "html_url": "https://github.com/acme/app/issues/1", "state": "open",
            "labels": [{"name": "powerqueue"}, {"name": "bug"}], "created_at": chrono::Utc::now(),
        }),
        comments: Vec::new(),
        operations: Vec::new(),
    })));
    Mock::given(wiremock::matchers::any()).respond_with(remote_state.clone()).mount(&server).await;

    // Push to a disposable local origin, exercising the actual git push/remove flow.
    let repo = env.root.path().join("repo");
    let origin = env.root.path().join("origin.git");
    git(env.root.path(), &["init", "--bare", "-q", "origin.git"]);
    git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&repo, &["push", "-u", "origin", "main"]);
    let config_path = env.home().join("config/config.toml");
    let mut cfg = powerqueue::config::Config::from_toml(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    let gh_state = env.root.path().join("fakestate");
    std::fs::create_dir_all(&gh_state).unwrap();
    env.vars.push(("FAKE_GH_DIR".into(), gh_state.display().to_string()));
    cfg.scheduler.gh_binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-gh.sh").display().to_string();
    cfg.scheduler.pr_poll_secs = 1;
    if with_review {
        std::fs::write(gh_state.join("pr-list.json"), r#"[{"url":"https://github.com/acme/app/pull/9"}]"#).unwrap();
        std::fs::write(gh_state.join("pr-9.json"), gh_pr(9, "OPEN", "MERGEABLE")).unwrap();
    }
    cfg.repo.default_branch = Some("main".into());
    cfg.cleanup.push_branch = true;
    cfg.github.enabled = true;
    cfg.github.repository = "acme/app".into();
    cfg.github.endpoint = server.uri();
    cfg.github.in_progress_label = Some("agent:working".into());
    cfg.github.done_label = Some("agent:review".into());
    cfg.github.close_on_complete = close_on_complete;
    // Keep real account usage probes out of this isolated daemon.
    cfg.budget.probe_interval_mins = 0;
    std::fs::write(&config_path, cfg.to_toml().unwrap()).unwrap();

    env.run_ok(&["secrets", "set", "github", "github-e2e-token"]);
    env.run_ok(&["github", "test"]);
    let preview: serde_json::Value = serde_json::from_str(&env.run_ok(&["--json", "github", "sync"])).unwrap();
    assert_eq!(preview["changes"].as_array().unwrap().len(), 1);
    assert_eq!(env.task_state("acme/app#1"), "?", "dry run must not import the issue");

    // No `add` or `sync --apply`: daemon polling itself must create and launch it.
    env.start_daemon();
    if with_review {
        assert_eq!(env.wait_for_state("acme/app#1", "in_review", Duration::from_secs(60)), "in_review", "{}", env.daemon_log());
        let held = env.wait_for_show("acme/app#1", |v| has_event(v, "session.released"));
        assert!(has_event(&held, "session.released"));
        assert_eq!(remote_state.0.lock().unwrap().issue["state"], "open", "do not close before merge");
        std::fs::write(gh_state.join("pr-9.json"), gh_pr(9, "MERGED", "UNKNOWN")).unwrap();
    }
    assert_eq!(env.wait_for_state("acme/app#1", "completed", Duration::from_secs(60)), "completed", "{}", env.daemon_log());
    let show = env.wait_for_show("acme/app#1", |v| {
        v["usage"]["output_tokens"].as_u64().unwrap_or(0) >= 1400
            && has_event(v, "cleanup.done")
            && v["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "github.comment").count() == 2
    });
    for expected in
        ["task.created", "worktree.ready", "session.launched", "cleanup.pushed", "cleanup.done", "github.label", "github.comment"]
    {
        assert!(has_event(&show, expected), "missing {expected}: {show}; log: {}", env.daemon_log());
    }
    assert!(if with_review { has_event(&show, "review.merged") } else { completed_event(&event_kinds(&show)) });
    assert_eq!(has_event(&show, "github.closed"), close_on_complete);
    assert!(!has_event(&show, "github.update_failed"), "{show}");
    assert!(!has_event(&show, "github.update_skipped"), "{show}");
    assert!(show["usage"]["output_tokens"].as_u64().unwrap_or(0) >= 1400, "{show}");
    let cleanup = show["events"].as_array().unwrap().iter().find(|e| e["kind"] == "cleanup.done").unwrap();
    for field in ["pushed", "worktree_removed"] {
        assert_eq!(cleanup["data"][field], true, "{cleanup}");
    }
    // Review hand-off kills the window before cleanup; check the actual pane,
    // rather than requiring cleanup itself to have killed it a second time.
    let session = &show["sessions"][0]["session"];
    let tmux = powerqueue::tmux::Tmux::new("tmux", Some(env.socket.clone()));
    assert!(tmux.find_pane(session["tmux_session"].as_str().unwrap(), session["pane_id"].as_str().unwrap()).unwrap().is_none());
    assert_eq!(show["task"]["source"]["kind"], "github");
    assert_eq!(show["task"]["attempts"], 1);
    let worktree = show["events"].as_array().unwrap().iter().find(|e| e["kind"] == "worktree.ready").unwrap()["data"]["path"]
        .as_str()
        .unwrap();
    assert!(!Path::new(worktree).exists());
    let branch = show["task"]["branch"].as_str().unwrap();
    git(&origin, &["cat-file", "-e", &format!("{branch}:FAKE_CLAUDE_TOUCHED.txt")]);
    let id = show["task"]["id"].as_str().unwrap();
    let prompt = std::fs::read_to_string(env.home().join(format!("state/tasks/{id}/prompt.md"))).unwrap();
    assert!(prompt.contains("https://github.com/acme/app/issues/1"));
    assert!(prompt.contains("Make the fixture commit"));

    for _ in 0..2 {
        let synced: serde_json::Value = serde_json::from_str(&env.run_ok(&["--json", "github", "sync", "--apply"])).unwrap();
        assert!(synced["changes"].as_array().unwrap().is_empty(), "completed tasks must not be reimported");
    }
    let status: serde_json::Value = serde_json::from_str(&env.run_ok(&["--json", "status", "--all"])).unwrap();
    assert_eq!(status["tasks"].as_array().unwrap().len(), 1, "PRs and excluded issues must not enter the queue");
    let remote = remote_state.0.lock().unwrap();
    assert_eq!(remote.issue["state"], if close_on_complete { "closed" } else { "open" });
    let labels: Vec<_> = remote.issue["labels"].as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap()).collect();
    assert_eq!(labels, vec!["powerqueue", "bug", "agent:review"]);
    assert_eq!(remote.comments.len(), 2, "start and completion comments only");
    assert!(remote.comments[0].contains("started attempt 1"));
    assert!(remote.comments[1].contains("fake-claude finished"));
    let mut operations = vec!["label:agent:working", "comment", "label:agent:review", "remove:agent:working"];
    if close_on_complete {
        operations.push("close");
    }
    operations.push("comment");
    assert_eq!(remote.operations, operations);
    drop(remote);
    // Shut down while the API fixture is still serving requests.
    drop(env);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_issue_runs_to_completion_and_closes() {
    github_lifecycle_e2e(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_issue_runs_to_completion_and_stays_open_for_review() {
    github_lifecycle_e2e(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_issue_waits_for_pr_merge_before_closing() {
    github_lifecycle_e2e(true, true).await;
}
