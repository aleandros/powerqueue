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
    _repo: tempfile::TempDir,
    state: tempfile::TempDir,
}

impl Env {
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
        std::fs::write(config_dir.join("PRIORITY.md"), "## Low\n- label: chore\n").unwrap();
        let env = Env { home, _repo: repo, state };
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

    fn priority(&self) -> String {
        std::fs::read_to_string(self.home.path().join("config/PRIORITY.md")).unwrap()
    }
    fn config(&self) -> String {
        std::fs::read_to_string(self.home.path().join("config/config.toml")).unwrap()
    }
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
    fn draft_with_status(&self, status: &str) -> PathBuf {
        let found: Vec<PathBuf> = self.drafts().into_iter().filter(|d| self.meta(d)["status"] == status).collect();
        assert_eq!(found.len(), 1, "drafts with status {status}: {found:?}");
        found.into_iter().next().unwrap()
    }
    fn meta(&self, draft: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(draft.join("meta.json")).unwrap()).unwrap()
    }
    fn state_file(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.state.path().join(name)).ok()
    }
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
        .stdout(predicate::str::contains("Change to PRIORITY.md"))
        .stdout(predicate::str::contains("+- FAKE-1: critical"))
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
    assert!(env.priority().contains("- FAKE-1: critical"));
    assert!(env.config().contains("max_concurrent = 3"));
    let drafts = env.drafts();
    assert_eq!(drafts.len(), 1);
    let draft = &drafts[0];
    assert_eq!(std::fs::read_to_string(draft.join("original/PRIORITY.md")).unwrap(), before_priority);
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
    assert!(prompt.contains("# Reference: PRIORITY.md grammar"));
    assert!(prompt.contains("### `[scheduler]`"), "config reference is embedded");
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
        .stdout(predicate::str::contains("PRIORITY.md"));
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
        .stdout(predicate::str::contains("Change to PRIORITY.md"))
        .stdout(predicate::str::contains("proposed:"));
    assert_eq!(env.priority(), before);
    env.pq().args(["tune", "--apply", "-y", "--no-budget"]).assert().success().stdout(predicate::str::contains("applied:"));
    assert!(env.priority().contains("- FAKE-1: critical"));
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
        .stderr(predicate::str::contains("PRIORITY.md line"))
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
fn tune_reports_no_change_failures_and_timeouts() {
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
    assert_eq!(env.meta(&failed)["problems"][0], "`".to_string() + fixture().to_str().unwrap() + "` exited with 1");

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
    // The fake edits both drafts, but only PRIORITY.md is in scope.
    let out =
        env.tune("edit").args(["--json", "tune", "FAKE-1 first", "-y", "--no-budget", "--scope", "priority"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "applied");
    assert_eq!(v["applied"], true);
    assert_eq!(v["scope"], "priority");
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["name"], "PRIORITY.md");
    assert_eq!(files[0]["changed"], true);
    assert!(files[0]["diff"].as_str().unwrap().contains("+- FAKE-1: critical"));
    assert_eq!(v["simulation"]["rows"][0]["key"], "FAKE-1");
    assert_eq!(v["simulation"]["rows"][0]["criticality"], "critical");
    assert!(v["summary"].as_str().unwrap().contains("fake-tune"));
    assert_eq!(env.config(), before_config, "config.toml is out of scope");
    assert!(env.priority().contains("- FAKE-1: critical"));
}

#[test]
fn draft_flags_on_check_simulate_and_validate() {
    let env = Env::new();
    let draft_rules = env.home.path().join("draft.md");
    std::fs::write(&draft_rules, "## High\n- label: chore\n").unwrap();
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
