//! End-to-end smoke tests of the CLI binary against an isolated
//! `POWERQUEUE_HOME`. Nothing here needs tmux, git worktrees or the network.

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

fn pq(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("powerqueue").expect("binary builds");
    cmd.env("POWERQUEUE_HOME", home)
        .env("POWERQUEUE_SECRETS", "file")
        // Output is not a TTY, so colours are off; `NO_COLOR` is removed because
        // clap parses it as a bool and rejects the conventional `1`.
        .env_remove("NO_COLOR")
        .env_remove("LINEAR_API_KEY")
        .env_remove("JEV_API_KEY");
    cmd
}

fn write_minimal_config(home: &Path, repo: &Path) {
    let config_dir = home.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), format!("[repo]\npath = \"{}\"\n", repo.display())).unwrap();
}

#[test]
fn help_and_completions() {
    let home = tempfile::tempdir().unwrap();
    pq(home.path()).arg("--help").assert().success().stdout(
        predicate::str::contains("Linear tickets\ninto Claude Code sessions")
            .or(predicate::str::contains("Linear tickets into Claude Code")),
    );
    pq(home.path()).args(["completions", "zsh"]).assert().success().stdout(predicate::str::contains("#compdef powerqueue"));
}

#[test]
fn config_path_works_before_init() {
    let home = tempfile::tempdir().unwrap();
    pq(home.path())
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("config"))
        .stdout(predicate::str::contains(home.path().join("config").join("config.toml").display().to_string()));
    pq(home.path()).args(["--json", "config", "path"]).assert().success().stdout(predicate::str::contains("\"database\""));
}

#[test]
fn status_before_init_is_friendly() {
    let home = tempfile::tempdir().unwrap();
    pq(home.path()).arg("status").assert().code(1).stderr(predicate::str::contains("powerqueue init"));
}

#[test]
fn dashboard_before_init_is_friendly() {
    let home = tempfile::tempdir().unwrap();
    pq(home.path()).arg("dashboard").assert().code(1).stderr(predicate::str::contains("powerqueue init"));
}

#[test]
fn dashboard_without_a_tty_exits_2() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_minimal_config(home.path(), repo.path());
    // assert_cmd pipes stdin/stdout, so the TTY precondition fails before any
    // escape sequence is written.
    pq(home.path())
        .env("TERM", "xterm-256color")
        .arg("dashboard")
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("the dashboard needs an interactive terminal (stdin/stdout are not a TTY)"))
        .stderr(predicate::str::contains("powerqueue status"));
}

#[test]
fn dashboard_once_prints_a_frame() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_minimal_config(home.path(), repo.path());
    pq(home.path()).args(["add", "Snapshot me", "--key", "MAN-9"]).assert().success();

    pq(home.path())
        .env("LANG", "en_US.UTF-8")
        .args(["dashboard", "--once"])
        .assert()
        .success()
        .stdout(predicate::str::contains("powerqueue"))
        .stdout(predicate::str::contains("daemon not running"))
        .stdout(predicate::str::contains("MAN-9"))
        .stdout(predicate::str::contains("Snapshot me"))
        .stdout(predicate::str::contains("┌"));

    // A non-UTF-8 locale switches to ASCII symbols and borders.
    let out = pq(home.path()).env("LC_ALL", "C").args(["dashboard", "--once"]).assert().success().get_output().stdout.clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.is_ascii(), "{text}");
    assert!(text.contains("+-") && text.contains("* daemon not running"), "{text}");

    pq(home.path())
        .args(["--json", "dashboard", "--once", "--ascii"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"daemon_alive\": false"))
        .stdout(predicate::str::contains("\"key\": \"MAN-9\""));
}

#[test]
fn add_then_status_and_show() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_minimal_config(home.path(), repo.path());

    pq(home.path())
        .args(["add", "Fix the login flow", "--key", "MAN-1", "--criticality", "high", "--label", "auth"])
        .assert()
        .success()
        .stdout(predicate::str::contains("added MAN-1"));

    pq(home.path())
        .args(["add", "Paused one", "--paused", "--description", "-", "--json"])
        .write_stdin("details from stdin")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"state\": \"paused\""))
        .stdout(predicate::str::contains("details from stdin"));

    pq(home.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("MAN-1"))
        .stdout(predicate::str::contains("Fix the login flow"))
        .stdout(predicate::str::contains("daemon"));

    pq(home.path()).args(["--json", "status"]).assert().success().stdout(predicate::str::contains("\"key\": \"MAN-1\""));

    pq(home.path())
        .args(["task", "show", "MAN-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("MAN-1"))
        .stdout(predicate::str::contains("criticality"))
        .stdout(predicate::str::contains("task.created"));

    pq(home.path()).args(["task", "explain", "man-1"]).assert().success().stdout(predicate::str::contains("score"));

    // Duplicate key is a clean error.
    pq(home.path()).args(["add", "Again", "--key", "MAN-1"]).assert().code(1).stderr(predicate::str::contains("already exists"));
}

#[test]
fn conditional_model_rows_are_reported_by_priority_commands() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_minimal_config(home.path(), repo.path());
    std::fs::write(
        home.path().join("config").join("PRIORITY.md"),
        "## High\n- source: manual\n\n## Models\n- if label: model/fable: fable\n- high: opus\n",
    )
    .unwrap();
    pq(home.path()).args(["add", "Grouped", "--key", "G-1", "--label", "model/fable"]).assert().success();
    pq(home.path()).args(["add", "Loose", "--key", "G-2", "--label", "fable"]).assert().success();

    pq(home.path()).args(["task", "show", "G-1"]).assert().success().stdout(predicate::str::contains("model/fable"));
    pq(home.path())
        .args(["priority", "explain", "G-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("model:       fable (if label: model/fable)"));
    pq(home.path())
        .args(["priority", "explain", "G-2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("model:       opus (high row)"));
    pq(home.path())
        .args(["priority", "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("models line 5: if label: model/fable → fable"));
    pq(home.path())
        .args(["priority", "simulate", "--no-budget"])
        .assert()
        .success()
        .stdout(predicate::str::contains("fable (if label: model/fable)"));
}

#[test]
fn offline_control_commands_apply_directly() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_minimal_config(home.path(), repo.path());
    pq(home.path()).args(["add", "Thing", "--key", "T-1"]).assert().success();

    pq(home.path()).args(["task", "pause", "T-1"]).assert().success().stdout(predicate::str::contains("applied directly"));
    pq(home.path())
        .args(["--json", "task", "show", "T-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"state\": \"paused\""));
    pq(home.path()).args(["task", "resume", "T-1"]).assert().success();
    pq(home.path()).args(["task", "cancel", "T-1"]).assert().success();
    pq(home.path()).arg("status").assert().success().stdout(predicate::str::contains("no open tasks"));
    pq(home.path()).args(["status", "--all"]).assert().success().stdout(predicate::str::contains("cancelled"));
    pq(home.path()).args(["task", "retry", "T-1"]).assert().success();
    pq(home.path()).args(["task", "model", "T-1", "opus"]).assert().success().stdout(predicate::str::contains("opus"));
    // Another provider's model is accepted; a disabled provider only warns.
    pq(home.path())
        .args(["task", "model", "T-1", "gpt-6-astra"])
        .assert()
        .success()
        .stdout(predicate::str::contains("will use gpt-6-astra (codex)"))
        .stderr(predicate::str::contains("codex is disabled in config"));
    pq(home.path())
        .args(["task", "show", "T-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("forced: gpt-6-astra (codex)"));
    pq(home.path())
        .args(["--json", "task", "show", "T-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"provider\": \"codex\""));
    pq(home.path()).args(["--json", "status"]).assert().success().stdout(predicate::str::contains("\"provider\": \"codex\""));
    pq(home.path()).arg("status").assert().success().stdout(predicate::str::contains("gpt-6-astra (codex)*"));
    pq(home.path())
        .args(["task", "model", "T-1", "llama"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown model `llama`"))
        .stderr(predicate::str::contains("fable|opus|sonnet|haiku"));
    pq(home.path()).args(["task", "model", "T-1", "auto"]).assert().success();
    pq(home.path()).args(["task", "complete", "T-1"]).assert().code(1).stderr(predicate::str::contains("cannot complete"));
    pq(home.path()).args(["task", "show", "nope"]).assert().code(1).stderr(predicate::str::contains("no task matches"));
}

#[test]
fn secrets_list_with_file_backend() {
    let home = tempfile::tempdir().unwrap();
    pq(home.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("linear_api_key"))
        .stdout(predicate::str::contains("jev_api_key"))
        .stdout(predicate::str::contains("secrets.toml"));
    pq(home.path())
        .args(["secrets", "set", "linear", "lin_api_1234567890"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stored"));
    pq(home.path())
        .args(["secrets", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("lin_************90"))
        .stdout(predicate::str::contains("file"));
    pq(home.path()).args(["secrets", "unset", "linear"]).assert().success();
    pq(home.path()).args(["secrets", "set", "openai", "x"]).assert().code(1).stderr(predicate::str::contains("unknown secret"));
}

#[test]
fn config_validate_and_show() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_minimal_config(home.path(), repo.path());
    pq(home.path()).args(["config", "validate"]).assert().success().stdout(predicate::str::contains("is valid"));
    pq(home.path()).args(["config", "show"]).assert().success().stdout(predicate::str::contains("[repo]"));

    std::fs::write(repo.path().join(".powerqueue.toml"), "bogus = 1\n").unwrap();
    pq(home.path()).args(["config", "validate"]).assert().code(1).stderr(predicate::str::contains("bogus"));

    let config_dir = home.path().join("config");
    std::fs::write(config_dir.join("config.toml"), "[repo]\npath = \"/x\"\n[scheduler]\nmax_concurrent = 0\n").unwrap();
    pq(home.path()).args(["config", "validate"]).assert().code(1).stderr(predicate::str::contains("max_concurrent"));
}

#[test]
fn logs_without_files_and_events() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_minimal_config(home.path(), repo.path());
    pq(home.path()).args(["add", "Logged", "--key", "L-1"]).assert().success();
    pq(home.path())
        .args(["logs", "--events", "--task", "L-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("task.created"));
    // The daemon log file is created lazily by tracing; a fresh install may
    // have an (empty) file or none at all, both of which must not crash.
    pq(home.path()).args(["logs", "-n", "5"]).assert().code(predicate::in_iter([0, 1]));
}

#[test]
fn priority_simulate_ranks_without_writing() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_minimal_config(home.path(), repo.path());
    std::fs::write(
        home.path().join("config").join("PRIORITY.md"),
        "## Critical\n- label: incident\n\n## Low\n- label: chore\n\n## Overrides\n- manual-skip: skip\n\n## Models\n- critical: fable\n- low: sonnet\n",
    )
    .unwrap();
    for (title, key, label) in
        [("Fix outage", "INC-1", "incident"), ("Tidy docs", "CH-2", "chore"), ("skip me", "manual-skip", "x")]
    {
        pq(home.path()).args(["add", title, "-k", key, "-l", label]).assert().success();
    }

    // Table: the incident is first and would start, the chore is low, the override skips.
    pq(home.path())
        .args(["priority", "simulate", "--reasons"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing was written"))
        .stdout(predicate::str::contains("normal → critical"))
        .stdout(predicate::str::contains("normal → low"))
        .stdout(predicate::str::contains("skip"))
        .stdout(predicate::str::contains("matched rule at line 2 (label: incident)"));

    // JSON: ranking order and the policy's model.
    let out = pq(home.path()).args(["--json", "priority", "simulate"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["key"], "INC-1");
    assert_eq!(rows[0]["criticality"], "critical");
    assert_eq!(rows[0]["model"], "fable");
    assert_eq!(rows[0]["would_start_now"], true);
    assert_eq!(rows[1]["key"], "CH-2");
    assert_eq!(rows[2]["key"], "manual-skip");
    assert_eq!(rows[2]["skip"], true);
    assert_eq!(rows[2]["model"], serde_json::Value::Null);

    // Nothing was stored: the tasks keep their default criticality until the daemon scores them.
    let out = pq(home.path()).args(["--json", "status"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("\"critical\""), "{text}");

    // A draft file is used instead of the live one, and a missing draft is an error.
    let draft = home.path().join("draft.md");
    std::fs::write(&draft, "## High\n- label: chore\n").unwrap();
    pq(home.path())
        .args(["priority", "simulate", "--file", draft.to_str().unwrap(), "--no-budget", "-n", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CH-2"))
        .stdout(predicate::str::contains("normal → high"))
        .stdout(predicate::str::contains("The daemon still uses the live file"));
    pq(home.path()).args(["priority", "simulate", "--file", "/nonexistent/rules.md"]).assert().code(1);
}
