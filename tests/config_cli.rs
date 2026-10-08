//! `init --non-interactive` followed by `config get/set/unset` and
//! `init --reconfigure`, against an isolated `POWERQUEUE_HOME`. Needs `git`
//! for the repository check; skips itself otherwise.

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

fn pq(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("powerqueue").expect("binary builds");
    cmd.env("POWERQUEUE_HOME", home)
        .env("POWERQUEUE_SECRETS", "file")
        .env_remove("NO_COLOR")
        .env_remove("LINEAR_API_KEY")
        .env_remove("JEV_API_KEY");
    cmd
}

fn git_repo() -> Option<tempfile::TempDir> {
    which::which("git").ok()?;
    let dir = tempfile::tempdir().unwrap();
    let ok = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(dir.path())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(dir)
}

#[test]
fn init_with_permission_mode_then_config_get_set_unset() {
    let Some(repo) = git_repo() else {
        eprintln!("git not available; skipping");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let config_file = home.path().join("config").join("config.toml");

    pq(home.path())
        .args(["init", "--non-interactive", "--no-linear", "--repo"])
        .arg(repo.path())
        .args(["--permission-mode", "auto"])
        .assert()
        .success()
        .stdout(predicate::str::contains("permission mode auto"))
        .stdout(predicate::str::contains("--reconfigure"));
    assert!(config_file.exists());

    pq(home.path())
        .args(["config", "get", "claude.permission_mode"])
        .assert()
        .success()
        .stdout(predicate::str::diff("\"auto\"\n"));
    pq(home.path())
        .args(["--json", "config", "get", "claude.permission_mode"])
        .assert()
        .success()
        .stdout(predicate::str::diff("\"auto\"\n"));
    pq(home.path()).args(["config", "get", "linear.enabled"]).assert().success().stdout(predicate::str::diff("false\n"));

    pq(home.path())
        .args(["config", "set", "scheduler.max_concurrent", "3"])
        .assert()
        .success()
        .stdout(predicate::str::contains("set scheduler.max_concurrent = 3"))
        .stdout(predicate::str::contains("no daemon running"));
    pq(home.path()).args(["config", "get", "scheduler.max_concurrent"]).assert().success().stdout(predicate::str::diff("3\n"));

    // Arrays and the --json form.
    pq(home.path())
        .args(["--json", "config", "set", "linear.team_keys", "[\"ENG\", \"OPS\"]"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"ok\":true"));
    pq(home.path())
        .args(["config", "get", "linear.team_keys"])
        .assert()
        .success()
        .stdout(predicate::str::diff("[\"ENG\", \"OPS\"]\n"));

    // An invalid value is rejected and the file is left as it was.
    let before = std::fs::read_to_string(&config_file).unwrap();
    pq(home.path())
        .args(["config", "set", "claude.permission_mode", "bogus"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("permission_mode"))
        .stderr(predicate::str::contains("nothing written"));
    assert_eq!(std::fs::read_to_string(&config_file).unwrap(), before);
    pq(home.path()).args(["config", "set", "claude.nope", "1"]).assert().code(1).stderr(predicate::str::contains("nope"));
    assert_eq!(std::fs::read_to_string(&config_file).unwrap(), before);
    pq(home.path())
        .args(["config", "get", "claude.permission_mode"])
        .assert()
        .success()
        .stdout(predicate::str::diff("\"auto\"\n"));

    // Unset falls back to the default.
    pq(home.path())
        .args(["config", "unset", "scheduler.max_concurrent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unset scheduler.max_concurrent (now 2)"));
    pq(home.path()).args(["config", "get", "scheduler.max_concurrent"]).assert().success().stdout(predicate::str::diff("2\n"));
    pq(home.path())
        .args(["config", "unset", "scheduler.max_concurrent"])
        .assert()
        .success()
        .stdout(predicate::str::contains("default already applies"));
    pq(home.path())
        .args(["config", "unset", "repo.path"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("repo.path is empty"));
    pq(home.path()).args(["config", "validate"]).assert().success();
}

#[test]
fn init_rejects_bad_permission_mode_and_reconfigure_keeps_other_settings() {
    let Some(repo) = git_repo() else {
        eprintln!("git not available; skipping");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    pq(home.path())
        .args(["init", "--non-interactive", "--no-linear", "--repo"])
        .arg(repo.path())
        .args(["--permission-mode", "yolo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("yolo"))
        .stderr(predicate::str::contains("acceptEdits"));
    assert!(!home.path().join("config").join("config.toml").exists());

    pq(home.path())
        .args(["init", "--non-interactive", "--reconfigure"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("nothing to reconfigure"));

    pq(home.path()).args(["init", "--non-interactive", "--no-linear", "--repo"]).arg(repo.path()).assert().success();
    pq(home.path())
        .args(["config", "get", "claude.permission_mode"])
        .assert()
        .success()
        .stdout(predicate::str::diff("\"acceptEdits\"\n"));
    pq(home.path()).args(["config", "set", "scheduler.max_concurrent", "3"]).assert().success();
    pq(home.path()).args(["config", "set", "repo.setup", "[\"make\"]"]).assert().success();

    // Without --reconfigure or --force an existing config is not touched.
    pq(home.path())
        .args(["init", "--non-interactive", "--no-linear"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--reconfigure"));

    pq(home.path())
        .args(["init", "--non-interactive", "--reconfigure", "--permission-mode", "bypassPermissions"])
        .assert()
        .success()
        .stdout(predicate::str::contains("permission mode bypassPermissions"));
    pq(home.path())
        .args(["config", "get", "claude.permission_mode"])
        .assert()
        .success()
        .stdout(predicate::str::diff("\"bypassPermissions\"\n"));
    pq(home.path()).args(["config", "get", "scheduler.max_concurrent"]).assert().success().stdout(predicate::str::diff("3\n"));
    pq(home.path()).args(["config", "get", "repo.setup"]).assert().success().stdout(predicate::str::diff("[\"make\"]\n"));
    pq(home.path()).args(["config", "get", "linear.enabled"]).assert().success().stdout(predicate::str::diff("false\n"));
}

#[test]
fn init_with_provider_flag_enables_codex() {
    let Some(repo) = git_repo() else {
        eprintln!("git not available; skipping");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    pq(home.path())
        .args(["init", "--non-interactive", "--no-linear", "--repo"])
        .arg(repo.path())
        .args(["--provider", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("providers      claude, codex"));
    pq(home.path())
        .args(["config", "get", "budget.providers.codex.enabled"])
        .assert()
        .success()
        .stdout(predicate::str::diff("true\n"));
    pq(home.path())
        .args(["config", "get", "budget.providers.gemini.enabled"])
        .assert()
        .success()
        .stdout(predicate::str::diff("false\n"));
    // `--provider` also accepts the gemini aliases; reconfigure keeps codex on.
    pq(home.path())
        .args(["init", "--reconfigure", "--non-interactive", "--provider", "agy"])
        .assert()
        .success()
        .stdout(predicate::str::contains("providers      claude, codex, gemini"));
    pq(home.path())
        .args(["config", "get", "budget.providers.gemini.enabled"])
        .assert()
        .success()
        .stdout(predicate::str::diff("true\n"));
    pq(home.path())
        .args(["init", "--non-interactive", "--no-linear", "--provider", "llama", "--repo"])
        .arg(repo.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("codex"));
}

fn git(cwd: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn repo_overrides_are_attributed_validated_and_read_from_the_default_branch() {
    let Some(repo) = git_repo() else {
        eprintln!("git not available; skipping");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    pq(home.path()).args(["init", "--non-interactive", "--no-linear", "--repo"]).arg(repo.path()).assert().success();
    std::fs::create_dir_all(repo.path().join("docs")).unwrap();
    std::fs::write(
        repo.path().join(".powerqueue.toml"),
        "priority_file = \"docs/PRIORITY.md\"\n[linear]\nexcluded_labels = [\"agent/skip\"]\n[scheduler]\nmax_concurrent = 3\n",
    )
    .unwrap();
    std::fs::write(repo.path().join("docs/PRIORITY.md"), "## High\n- source: manual\n").unwrap();

    // show: header names the keys, lines are marked, JSON carries the attribution.
    pq(home.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(working tree): priority.file, linear.excluded_labels, scheduler.max_concurrent"))
        .stdout(predicate::str::contains("max_concurrent = 3  # .powerqueue.toml\n"))
        .stdout(predicate::str::contains("max_attempts = 3\n"));
    let out = pq(home.path()).args(["--json", "config", "show"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["repo_overrides"]["source"], "working-tree");
    assert_eq!(v["repo_overrides"]["rev"], serde_json::Value::Null);
    assert_eq!(v["repo_overrides"]["keys"][2], "scheduler.max_concurrent");
    assert_eq!(v["scheduler"]["max_concurrent"], 3);
    pq(home.path())
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(working tree) is valid and sets priority.file, linear.excluded_labels"));
    pq(home.path()).args(["priority", "path"]).assert().success().stdout(predicate::str::contains("docs/PRIORITY.md"));
    pq(home.path()).args(["priority", "check"]).assert().success().stdout(predicate::str::contains("1 rule(s)"));

    // A bad value set by the repo fails validation.
    std::fs::write(repo.path().join(".powerqueue.toml"), "[scheduler]\nmax_concurrent = 0\n").unwrap();
    pq(home.path())
        .args(["config", "validate"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("scheduler.max_concurrent must be >= 1"));

    // default-branch: the committed file counts, the working tree does not.
    std::fs::write(
        repo.path().join(".powerqueue.toml"),
        "priority_file = \"docs/PRIORITY.md\"\n[scheduler]\nmax_concurrent = 3\n",
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "config"]);
    std::fs::write(repo.path().join(".powerqueue.toml"), "[scheduler]\nmax_concurrent = 8\n").unwrap();
    std::fs::write(repo.path().join("docs/PRIORITY.md"), "## Low\n- source: manual\n## Low\n- label: x\n").unwrap();
    pq(home.path()).args(["config", "set", "repo.overrides_from", "default-branch"]).assert().success();
    pq(home.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(main): priority.file, scheduler.max_concurrent"))
        .stdout(predicate::str::contains("max_concurrent = 3  # .powerqueue.toml\n"));
    pq(home.path())
        .args(["priority", "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("docs/PRIORITY.md (main): 1 rule(s)"));
    pq(home.path()).args(["priority", "show"]).assert().success().stdout(predicate::str::contains("(main)"));
    pq(home.path()).args(["config", "validate"]).assert().success().stdout(predicate::str::contains("(main) is valid"));
    pq(home.path())
        .args(["tune", "--dry-run", "make chores low"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("set by the repository"));
}
