//! `powerqueue tune` end to end against an isolated `POWERQUEUE_HOME`, with
//! `tests/fixtures/fake-claude.sh` standing in for Claude Code's headless
//! mode (`FAKE_TUNE_MODE` selects what the "agent" does to the drafts).
//! Nothing here needs tmux, git worktrees or the network.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude.sh")
}

struct Env {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    state: tempfile::TempDir,
}

impl Env {
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn new() -> Env {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let config_dir = home.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            format!("[repo]\npath = \"{}\"\n\n[claude]\nbinary = \"{}\"\n", repo.path().display(), fixture().display()),
        )
        .unwrap();
        std::fs::write(config_dir.join("priority.toml"), "version = 1\ndefault_criticality = \"normal\"\n[[criticality]]\nlevel = \"low\"\nall = [{ field = \"label\", op = \"eq\", value = \"chore\" }]\n[jev]\nenabled = false\nquestion = \"How important is it to ship this ticket this week?\"\nlevels = [\"can wait indefinitely\", \"nice to have\", \"important this week\", \"blocking customers or revenue\"]\n").unwrap();
        let env = Env { home, repo, state };
        for (title, key, label) in [("Fix outage", "FAKE-1", "incident"), ("Tidy docs", "CH-2", "chore")] {
            env.pq().args(["add", title, "-k", key, "-l", label]).assert().success();
        }
        env
    }

    fn pq(&self) -> Command {
        let mut cmd = Command::cargo_bin("powerqueue").expect("binary builds");
        cmd.env("POWERQUEUE_HOME", self.home.path())
            .env("POWERQUEUE_SECRETS", "file")
            .env("FAKE_CLAUDE_STATE_DIR", self.state.path())
            .env_remove("FAKE_TUNE_MODE")
            .env_remove("NO_COLOR")
            .env_remove("LINEAR_API_KEY")
            .env_remove("JEV_API_KEY");
        cmd
    }

    fn tune(&self, mode: &str) -> Command {
        let mut cmd = self.pq();
        cmd.env("FAKE_TUNE_MODE", mode);
        cmd
    }

    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn priority(&self) -> String {
        std::fs::read_to_string(self.home.path().join("config/priority.toml")).unwrap()
    }
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn config(&self) -> String {
        std::fs::read_to_string(self.home.path().join("config/config.toml")).unwrap()
    }
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn drafts(&self) -> Vec<PathBuf> {
        let dir = self.home.path().join("state/tune");
        let mut out: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd.map(|e| e.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        };
        out.sort();
        out
    }
    /// The draft whose `meta.json` has `status`, panicking when there is not exactly one.
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn draft_with_status(&self, status: &str) -> PathBuf {
        let found: Vec<PathBuf> = self.drafts().into_iter().filter(|d| self.meta(d)["status"] == status).collect();
        assert_eq!(found.len(), 1, "drafts with status {status}: {found:?}");
        found.into_iter().next().unwrap()
    }
    #[expect(
        clippy::unused_self,
        reason = "Keep this helper on its context so callers use the same interface as adjacent context-dependent operations."
    )]
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn meta(&self, draft: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(draft.join("meta.json")).unwrap()).unwrap()
    }
    fn state_file(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.state.path().join(name)).ok()
    }
}

#[test]
fn unassigned_tasks_do_not_break_tuning_or_become_launch_candidates() {
    use powerqueue::domain::{Criticality, Task, TaskSource, TaskState};

    let env = Env::new();
    let paths = powerqueue::paths::Paths::rooted(env.home.path());
    let store = powerqueue::store::Store::open(&paths.database()).unwrap();
    for state in [TaskState::Blocked, TaskState::Queued] {
        let mut task = Task::new(format!("UNASSIGNED-{state}"), "Needs routing", TaskSource::Manual);
        task.execution_project.clear();
        task.state = state;
        task.criticality = Criticality::High;
        task.score = 123.0;
        store.insert_task(&task).unwrap();
    }
    let before = store.list_tasks().unwrap();
    for budget_args in [vec![], vec!["--no-budget"]] {
        let out =
            env.pq().args(["--json", "priority", "simulate"]).args(budget_args).assert().success().get_output().stdout.clone();
        let sim: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let rows = sim["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 4);
        let unassigned: Vec<_> = rows.iter().filter(|r| r["key"].as_str().unwrap().starts_with("UNASSIGNED-")).collect();
        assert_eq!(unassigned.len(), 2);
        for row in unassigned {
            assert_eq!(row["criticality"], "high");
            assert_eq!(row["score"], 123.0);
            assert_eq!(row["schedulable"], false);
            assert_eq!(row["would_start_now"], false);
            assert!(row["model"].is_null());
            assert!(row["reasons"].to_string().contains("no execution project assigned"));
        }
    }
    env.tune("priority").args(["tune", "prioritize FAKE-1", "--dry-run"]).assert().success();
    let prompt = env.state_file("tune-prompt.md").unwrap();
    assert!(prompt.contains("UNASSIGNED-blocked"));
    assert!(prompt.contains("no execution project assigned"));
    assert_eq!(store.list_tasks().unwrap(), before, "simulation and dry-run must not change tasks");
}

#[test]
fn simulation_does_not_offer_unapproved_tasks_a_launch_slot() {
    use powerqueue::domain::{Task, TaskSource};

    let env = Env::new();
    env.pq().args(["config", "set", "scheduler.max_concurrent", "3"]).assert().success();
    let paths = powerqueue::paths::Paths::rooted(env.home.path());
    let store = powerqueue::store::Store::open(&paths.database()).unwrap();
    let task = Task::new(
        "example/repo#1",
        "Needs source approval",
        TaskSource::GitHub {
            repository: "example/repo".into(),
            number: 1,
            url: "https://github.com/example/repo/issues/1".into(),
        },
    );
    store.insert_task(&task).unwrap();
    let before = store.list_tasks().unwrap();
    for budget_args in [vec![], vec!["--no-budget"]] {
        let out =
            env.pq().args(["--json", "priority", "simulate"]).args(&budget_args).assert().success().get_output().stdout.clone();
        let sim: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let rows = sim["rows"].as_array().unwrap();
        let row = rows.iter().find(|row| row["key"] == task.key).unwrap();
        assert_eq!(row["schedulable"], false);
        assert_eq!(row["would_start_now"], false);
        assert!(row["model"].is_null());
        if !budget_args.is_empty() {
            assert_eq!(rows.iter().filter(|row| row["would_start_now"] == true).count(), 2);
        }
    }
    assert_eq!(store.list_tasks().unwrap(), before, "simulation must not change source approval or task state");
}

#[test]
fn tune_needs_an_instruction_and_a_claude_binary() {
    let env = Env::new();
    // No instruction and stdin is a pipe with nothing on it → empty instruction.
    env.pq().arg("tune").write_stdin("").assert().code(1).stderr(predicate::str::contains("instruction is empty"));
    env.pq().args(["config", "set", "claude.binary", "/definitely/not/claude"]).assert().success();
    env.pq()
        .args(["tune", "make FAKE-1 critical"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not found on PATH"))
        .stderr(predicate::str::contains("claude.binary"));
    assert!(env.drafts().is_empty(), "no draft is created when the binary is missing");
    // A non-Claude model is refused before anything runs.
    env.pq().args(["tune", "x", "-m", "gpt-6.1-sol"]).assert().code(1).stderr(predicate::str::contains("not a Claude model"));
}

#[test]
fn tune_proposes_applies_and_undoes() {
    let env = Env::new();
    let before_priority = env.priority();
    let before_config = env.config();

    let out = env
        .tune("edit")
        .args(["tune", "FAKE-1 must be critical", "-y", "--no-budget"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Claude:"))
        .stdout(predicate::str::contains("fake-tune (edit): made FAKE-1 critical."))
        .stdout(predicate::str::contains("Change to priority.toml"))
        .stdout(predicate::str::contains("+key = \"FAKE-1\""))
        .stdout(predicate::str::contains("Change to config.toml"))
        .stdout(predicate::str::contains("+max_concurrent = 3"))
        .stdout(predicate::str::contains("Simulated queue with"))
        .stdout(predicate::str::contains("normal → critical"))
        .stdout(predicate::str::contains("(max 3 concurrent"))
        .stdout(predicate::str::contains("applied:"))
        .stdout(predicate::str::contains("powerqueue tune --undo"))
        .get_output()
        .clone();
    let _ = out;

    // Live files changed, originals kept, metadata recorded.
    assert!(env.priority().contains("key = \"FAKE-1\""));
    assert!(env.config().contains("max_concurrent = 3"));
    let drafts = env.drafts();
    assert_eq!(drafts.len(), 1);
    let draft = &drafts[0];
    assert_eq!(std::fs::read_to_string(draft.join("original/priority.toml")).unwrap(), before_priority);
    assert_eq!(std::fs::read_to_string(draft.join("original/config.toml")).unwrap(), before_config);
    let meta = env.meta(draft);
    assert_eq!(meta["status"], "applied");
    assert_eq!(meta["instruction"], "FAKE-1 must be critical");
    assert_eq!(meta["scope"], "all");
    assert_eq!(meta["model"], "sonnet");
    assert_eq!(meta["agent"]["num_turns"], 2);
    assert!(meta["applied_at"].is_string());
    assert!(draft.join("result.json").exists());
    assert!(draft.join("CONTEXT.md").exists());

    // The session saw the right command line, prompt and installation.
    let argv = env.state_file("tune-argv.txt").unwrap();
    assert!(argv.contains("-p\n"), "{argv}");
    assert!(argv.contains("--model\nsonnet\n"), "{argv}");
    assert!(argv.contains("--output-format\njson\n"), "{argv}");
    assert!(argv.contains("Bash(powerqueue priority simulate*)"), "{argv}");
    let prompt = env.state_file("tune-prompt.md").unwrap();
    assert!(prompt.contains("## The request\n\nFAKE-1 must be critical"), "{prompt}");
    assert!(prompt.contains("Simulated queue with"), "the current simulation is in the prompt");
    assert!(prompt.contains("| FAKE-1 | queued |"), "{prompt}");
    assert!(prompt.contains("# Reference: priority.toml schema"));
    assert!(prompt.contains("## `[scheduler]`"), "config reference is embedded");
    assert_eq!(prompt, std::fs::read_to_string(draft.join("prompt.md")).unwrap());
    // `powerqueue` resolved through PATH inside the session, against the same home.
    assert_eq!(env.state_file("tune-check-exit.txt").unwrap().trim(), "0");
    assert_eq!(env.state_file("tune-validate-exit.txt").unwrap().trim(), "0");

    // The event is in the timeline.
    env.pq().args(["logs", "--events", "-n", "50"]).assert().success().stdout(predicate::str::contains("tune.applied"));

    // Undo restores both files.
    env.pq()
        .args(["tune", "--undo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("undone:"))
        .stdout(predicate::str::contains("priority.toml"));
    assert_eq!(env.priority(), before_priority);
    assert_eq!(env.config(), before_config);
    assert_eq!(env.meta(draft)["status"], "undone");
    env.pq().args(["tune", "--undo"]).assert().code(1).stderr(predicate::str::contains("no applied tune draft"));
}

#[test]
fn tune_dry_run_then_apply_latest() {
    let env = Env::new();
    let before = env.priority();
    env.tune("priority")
        .args(["tune", "FAKE-1 first", "--dry-run", "--no-budget", "--scope", "priority"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry run: nothing was applied"))
        .stdout(predicate::str::contains("powerqueue tune --apply"))
        .stdout(predicate::str::contains("· config.toml unchanged").not());
    assert_eq!(env.priority(), before);
    let drafts = env.drafts();
    assert_eq!(drafts.len(), 1);
    assert_eq!(env.meta(&drafts[0])["status"], "proposed");

    // doctor points at the pending proposal.
    env.pq().args(["doctor", "--offline"]).assert().stdout(predicate::str::contains("1 proposal(s) not applied"));

    // Apply the newest proposal without a terminal: needs -y.
    env.pq()
        .args(["tune", "--apply"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("Change to priority.toml"))
        .stdout(predicate::str::contains("proposed:"));
    assert_eq!(env.priority(), before);
    env.pq().args(["tune", "--apply", "-y", "--no-budget"]).assert().success().stdout(predicate::str::contains("applied:"));
    assert!(env.priority().contains("key = \"FAKE-1\""));
    assert_eq!(env.meta(&drafts[0])["status"], "applied");
    env.pq().args(["tune", "--apply", "-y"]).assert().code(1).stderr(predicate::str::contains("no proposed tune draft"));
    let dir = drafts[0].to_str().unwrap().to_string();
    env.pq().args(["tune", "--apply", &dir, "-y"]).assert().code(1).stderr(predicate::str::contains("already applied"));
}

#[test]
fn tune_without_a_terminal_leaves_a_proposal() {
    let env = Env::new();
    env.tune("edit")
        .args(["tune", "FAKE-1 first", "--no-budget"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("not applied (no terminal to ask on"));
    assert!(!env.priority().contains("FAKE-1"));
    assert_eq!(env.meta(&env.drafts()[0])["status"], "proposed");
}

#[test]
fn tune_rejects_invalid_drafts() {
    let env = Env::new();
    let (p, c) = (env.priority(), env.config());
    env.tune("invalid")
        .args(["tune", "break things", "-y", "--no-budget"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("invalid:"))
        .stderr(predicate::str::contains("priority.toml line"))
        .stderr(predicate::str::contains("scheduler.max_concurrent must be >= 1"));
    assert_eq!(env.priority(), p);
    assert_eq!(env.config(), c);
    let meta = env.meta(&env.drafts()[0]);
    assert_eq!(meta["status"], "invalid");
    assert!(meta["problems"].as_array().unwrap().len() >= 2);
    // An invalid draft cannot be applied by hand either.
    let dir = env.drafts()[0].to_str().unwrap().to_string();
    env.pq().args(["tune", "--apply", &dir, "-y"]).assert().code(1).stderr(predicate::str::contains("only proposed drafts"));
}

#[test]
fn tunereports_no_change_failures_and_timeouts() {
    let env = Env::new();
    env.tune("noop")
        .args(["tune", "do nothing", "-y", "--no-budget"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no change:"))
        .stdout(predicate::str::contains("fake-tune (noop)"));
    env.draft_with_status("unchanged");

    env.tune("fail")
        .args(["tune", "please fail", "-y"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("exited with 1"))
        .stderr(predicate::str::contains("simulated failure"))
        .stderr(predicate::str::contains("draft is kept"));
    let failed = env.draft_with_status("failed");
    assert_eq!(env.meta(&failed)["problems"][0], format!("`{}` exited with 1", fixture().display()));

    env.tune("hang")
        .env("FAKE_CLAUDE_IDLE_SECS", "30")
        .args(["tune", "take forever", "-y", "--timeout", "1"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("did not finish within 1s"));
    let timed_out: Vec<PathBuf> = env.drafts().into_iter().filter(|d| env.meta(d)["agent"]["timed_out"] == true).collect();
    assert_eq!(timed_out.len(), 1, "{timed_out:?}");
    assert_eq!(env.meta(&timed_out[0])["status"], "failed");
    assert_eq!(env.drafts().len(), 3);
    assert!(!env.priority().contains("FAKE-1"));
}

#[test]
fn tune_scope_and_json() {
    let env = Env::new();
    let before_config = env.config();
    // The fake edits both drafts, but only priority.toml is in scope.
    let out =
        env.tune("edit").args(["--json", "tune", "FAKE-1 first", "-y", "--no-budget", "--scope", "priority"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "applied");
    assert_eq!(v["applied"], true);
    assert_eq!(v["scope"], "priority");
    assert_eq!(v["simulation"]["max_concurrent"], 2, "out-of-scope config edits must not affect simulation");
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["name"], "priority.toml");
    assert_eq!(files[0]["changed"], true);
    assert!(files[0]["diff"].as_str().unwrap().contains("+key = \"FAKE-1\""));
    assert_eq!(v["simulation"]["rows"][0]["key"], "FAKE-1");
    assert_eq!(v["simulation"]["rows"][0]["criticality"], "critical");
    assert!(v["summary"].as_str().unwrap().contains("fake-tune"));
    assert_eq!(env.config(), before_config, "config.toml is out of scope");
    assert!(env.priority().contains("key = \"FAKE-1\""));
}

#[test]
fn draft_flags_on_check_simulate_and_validate() {
    let env = Env::new();
    let draft_rules = env.home.path().join("draft.md");
    std::fs::write(&draft_rules, "version = 1\ndefault_criticality = \"normal\"\n[[criticality]]\nlevel = \"high\"\nall = [{ field = \"label\", op = \"eq\", value = \"chore\" }]\n[jev]\nenabled = false\nquestion = \"How important is it to ship this ticket this week?\"\nlevels = [\"can wait indefinitely\", \"nice to have\", \"important this week\", \"blocking customers or revenue\"]\n").unwrap();
    env.pq()
        .args(["priority", "check", "--file", draft_rules.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 rule(s)"));
    env.pq().args(["priority", "check", "--file", "/nonexistent.md"]).assert().code(1);

    let draft_config = env.home.path().join("draft.toml");
    std::fs::write(&draft_config, format!("{}\n[scheduler]\nmax_concurrent = 5\n", env.config())).unwrap();
    env.pq()
        .args(["config", "validate", "--file", draft_config.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("draft.toml is valid"));
    env.pq()
        .args([
            "priority",
            "simulate",
            "--file",
            draft_rules.to_str().unwrap(),
            "--config",
            draft_config.to_str().unwrap(),
            "--no-budget",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("(max 5 concurrent"))
        .stdout(predicate::str::contains("normal → high"))
        .stdout(predicate::str::contains("The daemon still uses the live files"));

    std::fs::write(&draft_config, "[scheduler]\nmax_concurrent = 0\n").unwrap();
    env.pq()
        .args(["config", "validate", "--file", draft_config.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("scheduler.max_concurrent"));
    env.pq()
        .args(["priority", "simulate", "--config", draft_config.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("is not valid"));
    std::fs::write(&draft_config, "nope = [[[").unwrap();
    env.pq()
        .args(["--json", "config", "validate", "--file", draft_config.to_str().unwrap()])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("\"ok\":false"));
}

#[test]
fn intent_snapshot_staleness_and_prose_only_edits() {
    let env = Env::new();
    let intent = env.home.path().join("config/PRIORITY.md");
    let prose = "# Goals\nFAKE-1 should be critical because customers cannot work.\n";
    std::fs::write(&intent, prose).unwrap();
    let before = env.priority();
    env.tune("intent")
        .args(["tune", "translate intent", "--scope", "priority", "--dry-run", "--no-budget"])
        .assert()
        .success()
        .stdout(predicate::str::contains("intent has no applied proposal"));
    assert_eq!(env.priority(), before);
    let draft = env.draft_with_status("proposed");
    assert_eq!(std::fs::read_to_string(draft.join("PRIORITY.md")).unwrap(), prose);
    let prompt = env.state_file("tune-prompt.md").unwrap();
    assert!(prompt.contains(prose));
    assert!(prompt.contains("Only applied TOML affects scheduling"));
    std::fs::write(&intent, "Now run chores first.\n").unwrap();
    env.pq().args(["tune", "--apply", "-y"]).assert().code(1).stderr(predicate::str::contains("intent changed since drafting"));
    assert_eq!(env.priority(), before);
    std::fs::write(&intent, prose).unwrap();
    env.pq().args(["tune", "--apply", "-y"]).assert().success();
    let applied = env.priority();
    let paths = powerqueue::paths::Paths::rooted(env.home.path());
    assert_eq!(
        powerqueue::tune::intent_status(&paths, &paths.priority_file(), &intent).unwrap(),
        "unchanged since last applied proposal"
    );
    std::fs::write(&intent, "Chores must run first instead.\n").unwrap();
    assert!(
        powerqueue::tune::intent_status(&paths, &paths.priority_file(), &intent)
            .unwrap()
            .contains("intent changed since last applied proposal")
    );
    assert_eq!(env.priority(), applied, "prose alone never rewrites policy");
    env.pq().args(["priority", "explain", "FAKE-1"]).assert().success().stdout(predicate::str::contains("critical"));
    env.pq().args(["tune", "--undo"]).assert().success();
    assert_eq!(env.priority(), before);
    assert!(std::fs::read_to_string(intent).unwrap().contains("Chores must run first"));
}

#[test]
fn project_selection_simulation_apply_and_undo_are_isolated() {
    let env = Env::new();
    let repo = tempfile::tempdir().unwrap();
    let policy = repo.path().join("priority.toml");
    std::fs::write(&policy, "version = 1\ndefault_criticality = 'low'\n").unwrap();
    std::fs::write(repo.path().join(".powerqueue.toml"), "priority_file = 'priority.toml'\n").unwrap();
    std::fs::write(repo.path().join("PRIORITY.md"), "Give FAKE-1 precedence in this project.\n").unwrap();
    std::fs::write(
        env.home.path().join("config/config.toml"),
        format!("{}\n[projects.api.repo]\npath = '{}'\n", env.config(), repo.path().display()),
    )
    .unwrap();
    env.pq().args(["task", "project", "FAKE-1", "api"]).assert().success();
    let machine_before = env.priority();
    let project_before = std::fs::read_to_string(&policy).unwrap();
    env.tune("priority")
        .args(["tune", "x", "--project", "missing", "--dry-run"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown execution project"));
    assert_eq!((env.drafts()).len(), 0);
    let out = env
        .tune("priority")
        .args(["--json", "tune", "translate intent", "--project", "api", "--scope", "priority", "--dry-run", "--no-budget"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["project"], "api");
    assert_eq!(json["simulation"]["rows"].as_array().unwrap().len(), 1);
    assert_eq!(json["simulation"]["rows"][0]["key"], "FAKE-1");
    assert_eq!(json["simulation"]["rows"][0]["criticality"], "critical");
    assert!(env.state_file("tune-prompt.md").unwrap().contains("--project api --reasons"));
    env.pq().args(["tune", "--apply", "--project", "default", "-y"]).assert().code(1);
    env.pq().args(["tune", "--apply", "--project", "api", "-y"]).assert().success();
    assert_eq!(env.priority(), machine_before);
    assert!(std::fs::read_to_string(&policy).unwrap().contains("FAKE-1"));
    env.pq().args(["tune", "--undo", "--project", "api"]).assert().success();
    assert_eq!(std::fs::read_to_string(&policy).unwrap(), project_before);
    assert_eq!(env.priority(), machine_before);
    // A machine proposal must not replace api's independently configured policy.
    let out = env
        .tune("priority")
        .args(["--json", "tune", "machine intent", "--scope", "priority", "--dry-run", "--no-budget"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let rows = json["simulation"]["rows"].as_array().unwrap();
    assert_eq!(rows.iter().find(|r| r["key"] == "FAKE-1").unwrap()["criticality"], "low");
}

#[test]
fn committed_policy_apply_prepares_a_reviewable_change() {
    if which::which("git").is_err() {
        return;
    }
    let env = Env::new();
    let repo = env.repo.path();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(repo.join(".powerqueue.toml"), "priority_file = 'priority.toml'\n").unwrap();
    let before = "version = 1\ndefault_criticality = 'normal'\n";
    std::fs::write(repo.join("priority.toml"), before).unwrap();
    std::fs::write(repo.join("PRIORITY.md"), "FAKE-1 is customer critical.\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "initial"]);
    env.pq().args(["config", "set", "repo.overrides_from", "default-branch"]).assert().success();
    env.pq().args(["config", "set", "repo.default_branch", "main"]).assert().success();
    env.tune("priority")
        .args(["tune", "translate intent", "--scope", "priority", "-y", "--no-budget"])
        .assert()
        .success()
        .stdout(predicate::str::contains("review, commit and merge"));
    assert!(git(&["diff", "--", "priority.toml"]).contains("+key = \"FAKE-1\""));
    assert_eq!(git(&["show", "main:priority.toml"]), before);
    env.pq().args(["priority", "show"]).assert().success().stdout(predicate::str::contains("FAKE-1").not());
    env.pq().args(["tune", "--undo"]).assert().success();
    assert_eq!(std::fs::read_to_string(repo.join("priority.toml")).unwrap(), before);
    assert_eq!(git(&["diff", "--", "priority.toml"]), "");
    std::fs::write(repo.join("priority.toml"), "version = 1\ndefault_criticality = 'low'\n").unwrap();
    env.tune("priority")
        .args(["tune", "x", "--scope", "priority", "-y"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("commit/stash or restore"));
    assert!(std::fs::read_to_string(repo.join("priority.toml")).unwrap().contains("'low'"));
}

#[test]
fn apply_rejects_policy_edits_and_invalid_or_modified_intent_drafts() {
    let env = Env::new();
    env.tune("priority").args(["tune", "x", "--scope", "priority", "--dry-run"]).assert().success();
    let draft = env.draft_with_status("proposed");
    let before = env.priority();
    std::fs::write(env.home.path().join("config/priority.toml"), "version = 1\ndefault_criticality = 'low'\n").unwrap();
    env.pq().args(["tune", "--apply", "-y"]).assert().code(1).stderr(predicate::str::contains("policy changed since drafting"));
    std::fs::write(env.home.path().join("config/priority.toml"), &before).unwrap();
    std::fs::write(draft.join("PRIORITY.md"), "agent changed the intent").unwrap();
    env.pq().args(["tune", "--apply", "-y"]).assert().code(1).stderr(predicate::str::contains("read-only intent"));
    std::fs::write(draft.join("PRIORITY.md"), "").unwrap();
    std::fs::write(draft.join("priority.toml"), "version = 1\nunknown = true\n").unwrap();
    env.pq().args(["tune", "--apply", "-y"]).assert().code(1).stderr(predicate::str::contains("does not validate"));
    assert_eq!(env.priority(), before);
}

#[test]
fn custom_intent_and_agent_error_keep_live_policy() {
    let env = Env::new();
    let intent = env.home.path().join("goals.md");
    std::fs::write(&intent, "FAKE-1 should be critical.\n").unwrap();
    let before = env.priority();
    env.tune("intent")
        .args(["tune", "translate", "--intent", intent.to_str().unwrap(), "--scope", "priority", "--dry-run"])
        .assert()
        .success();
    let draft = env.draft_with_status("proposed");
    assert_eq!(env.meta(&draft)["intent_path"], intent.to_str().unwrap());
    env.tune("report-error").args(["tune", "translate", "--intent", intent.to_str().unwrap(), "-y"]).assert().code(1);
    assert_eq!(env.priority(), before);
    assert_eq!(env.meta(&env.draft_with_status("failed"))["agent"]["is_error"], true);
    env.tune("intent")
        .args(["tune", "translate", "--intent", "/missing/pq-intent.md", "-y"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("intent document"));
}

#[test]
fn invalid_overrides_refuse_all_policy_mutations() {
    let env = Env::new();
    let policy = env.priority();
    let config = env.config();
    std::fs::write(env.repo.path().join(".powerqueue.toml"), "priority_file = 'docs/priority.toml'\n[broken\n").unwrap();
    for args in
        [vec!["tune", "change something", "--dry-run"], vec!["tune", "--apply"], vec!["tune", "--undo"], vec!["priority", "edit"]]
    {
        env.pq().args(args).assert().failure().stderr(predicate::str::contains("invalid repository overrides"));
    }
    assert_eq!((env.drafts()).len(), 0);
    assert_eq!(env.priority(), policy);
    assert_eq!(env.config(), config);
}

#[test]
fn intent_only_policy_can_be_created_by_tune() {
    let env = Env::new();
    std::fs::remove_file(env.home.path().join("config/priority.toml")).unwrap();
    let intent = env.home.path().join("config/PRIORITY.md");
    std::fs::write(&intent, powerqueue::priority::intent_template()).unwrap();
    env.tune("priority").args(["tune", "make FAKE-1 critical", "--yes"]).assert().success();
    assert!(env.home.path().join("config/priority.toml").exists());
    assert_eq!(std::fs::read_to_string(intent).unwrap(), powerqueue::priority::intent_template());
}
