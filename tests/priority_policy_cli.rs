//! Migration and execution-project policy behavior through the real CLI.
use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

fn pq(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("powerqueue").unwrap();
    cmd.env("POWERQUEUE_HOME", home)
        .env("POWERQUEUE_SECRETS", "file")
        .env_remove("NO_COLOR")
        .env_remove("LINEAR_API_KEY")
        .env_remove("JEV_API_KEY")
        .env_remove("GITHUB_TOKEN");
    cmd
}

#[test]
fn migration_is_explicit_preserves_source_and_refuses_overwrite() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let old = config.join("PRIORITY.md");
    let new = config.join("priority.toml");
    let original = "## High\n- source: manual\n";
    std::fs::write(&old, original).unwrap();
    pq(home.path()).args(["priority", "check"]).assert().failure().stderr(predicate::str::contains("priority migrate"));
    pq(home.path()).args(["priority", "migrate", "--from"]).arg(&old).arg("--to").arg(&new).assert().success();
    pq(home.path()).args(["priority", "check"]).assert().success();
    assert_eq!(std::fs::read_to_string(&old).unwrap(), original);
    let converted = std::fs::read_to_string(&new).unwrap();
    pq(home.path()).args(["priority", "migrate", "--from"]).arg(&old).arg("--to").arg(&new).assert().failure();
    assert_eq!(std::fs::read_to_string(new).unwrap(), converted);
    pq(home.path())
        .args(["priority", "check", "--file"])
        .arg(old)
        .assert()
        .failure()
        .stdout(predicate::str::contains("Markdown rules are retired"));
}

#[test]
fn project_selection_explanation_and_simulation_agree() {
    let home = tempfile::tempdir().unwrap();
    let paths = powerqueue::paths::Paths::rooted(home.path());
    paths.ensure().unwrap();
    let mut cfg = powerqueue::config::Config::default();
    cfg.linear.enabled = false;
    for (id, level, model) in [("api", "critical", "fable"), ("web", "low", "haiku")] {
        let repo = home.path().join(id);
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join(".powerqueue.toml"), "priority_file = 'priority.toml'\n").unwrap();
        std::fs::write(
            repo.join("priority.toml"),
            format!("version = 1\ndefault_criticality = '{level}'\n[models]\n{level} = ['{model}']\n"),
        )
        .unwrap();
        let mut project = powerqueue::projects::ProjectConfig::default();
        project.repo.path = repo.display().to_string();
        cfg.projects.insert(id.into(), project);
    }
    cfg.save(&paths).unwrap();
    let store = powerqueue::store::Store::open(&paths.database()).unwrap();
    for id in ["api", "web"] {
        let mut task = powerqueue::domain::Task::new(id, "test", powerqueue::domain::TaskSource::Manual);
        task.execution_project = id.into();
        store.insert_task(&task).unwrap();
    }
    pq(home.path()).args(["priority", "check", "--project", "api"]).assert().success();
    pq(home.path())
        .args(["priority", "show", "--project", "web"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Default: low"));
    pq(home.path())
        .args(["priority", "path", "--project", "api"])
        .assert()
        .success()
        .stdout(predicate::str::contains("api/priority.toml"));
    for (id, level) in [("api", "critical"), ("web", "low")] {
        let output = pq(home.path()).args(["--json", "priority", "explain", id]).assert().success().get_output().stdout.clone();
        let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(json["evaluation"]["criticality"], level);
    }
    let output =
        pq(home.path()).args(["--json", "priority", "simulate", "--no-budget"]).assert().success().get_output().stdout.clone();
    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["rows"][0]["key"], "api");
    assert_eq!(json["rows"][0]["criticality"], "critical");
    assert_eq!(json["rows"][1]["criticality"], "low");
    let output = pq(home.path())
        .args(["--json", "priority", "simulate", "--no-budget", "--project", "web"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["rows"].as_array().unwrap().len(), 1);
    assert_eq!(json["rows"][0]["key"], "web");
}

#[test]
fn intent_prose_allows_initial_policy_but_legacy_directives_require_migration() {
    use powerqueue::config::RulesSource;
    let home = tempfile::tempdir().unwrap();
    let policy = home.path().join("priority.toml");
    let intent = home.path().join("PRIORITY.md");
    for text in [
        powerqueue::priority::intent_template(),
        "# Intent\n## Goals\n- Fix outages first.\n",
        "<!--\n## Models\n- low: haiku\n-->\n",
    ] {
        std::fs::write(&intent, text).unwrap();
        assert_eq!(RulesSource::File(policy.clone()).read().unwrap(), None);
    }
    for text in
        ["## Models\n- low: haiku\n", "## Default\n- normal\n", "## Jev\n- enabled: false\n", "## High\n- invalid condition\n"]
    {
        std::fs::write(&intent, text).unwrap();
        assert!(RulesSource::File(policy.clone()).read().unwrap_err().to_string().contains("priority migrate"));
    }
}
