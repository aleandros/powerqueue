//! Real CLI, mocked model endpoints, real SQLite and concurrent task updates.
use assert_cmd::Command;
use powerqueue::analysis::{AnalysisInput, AnalysisRecord, Classification, Proposal, record_key};
use powerqueue::config::Config;
use powerqueue::domain::{Task, TaskSource, TaskState};
use powerqueue::paths::Paths;
use powerqueue::store::Store;
use serde_json::{Value, json};
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
)]
fn pq(home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("powerqueue").unwrap();
    cmd.env("POWERQUEUE_HOME", home)
        .env("POWERQUEUE_SECRETS", "file")
        .env("NO_COLOR", "1")
        .env_remove("LINEAR_API_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("JEV_API_KEY")
        .env_remove("POWERQUEUE_ANALYSIS_API_KEY");
    cmd
}

#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
)]
fn setup(endpoint: &str) -> (tempfile::TempDir, Paths, Config, Store, Task) {
    let home = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(home.path());
    paths.ensure().unwrap();
    let mut cfg = Config::default();
    cfg.repo.path = home.path().join("repo").display().to_string();
    std::fs::create_dir_all(&cfg.repo.path).unwrap();
    cfg.linear.enabled = false;
    cfg.analysis.enabled = true;
    cfg.analysis.jev_first = false;
    cfg.analysis.projects.insert("default".into(), "API authentication".into());
    cfg.analysis.llm =
        Some(powerqueue::analysis::TextModelConfig { endpoint: endpoint.into(), model: "fixture".into(), ..Default::default() });
    cfg.save(&paths).unwrap();
    let store = Store::open(&paths.database()).unwrap();
    let mut task = Task::new(
        "GH-44",
        "Fix API login",
        TaskSource::GitHub {
            repository: "example/issues".into(),
            number: 44,
            url: "https://github.com/example/issues/44".into(),
        },
    );
    task.state = TaskState::Paused;
    task.github_admission = Some(powerqueue::domain::GitHubAdmission::denied("not approved"));
    store.insert_task(&task).unwrap();
    let task = store.get_task(task.id).unwrap().unwrap();
    (home, paths, cfg, store, task)
}

fn proposal() -> Proposal {
    Proposal {
        project: Some("default".into()),
        classification: Classification::Bug,
        confidence: 0.95,
        reasoning: "Authentication belongs to API".into(),
        plan: vec!["Reproduce login failure and test the fix".into()],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_analysis_is_advisory_persisted_visible_and_detects_stale_content() {
    let server = MockServer::start().await;
    Mock::given(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "stop_reason": "end_turn", "content": [{"type":"text", "text":serde_json::to_string(&proposal()).unwrap()}],
            "usage": {"input_tokens":200,"output_tokens":80}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (home, _, cfg, store, task) = setup(&format!("{}/messages", server.uri()));
    let output = pq(home.path())
        .env("POWERQUEUE_ANALYSIS_API_KEY", "test-only")
        .args(["--json", "task", "analyze", &task.key])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(result["repository_assigned"], false);
    assert_eq!(result["analysis"]["proposal"]["project"], "default");
    assert_eq!(store.get_task(task.id).unwrap().unwrap(), task);
    assert_eq!((store.list_sessions_for_task(task.id).unwrap()).len(), 0);
    let saved: AnalysisRecord = store.kv_get(&record_key(task.id)).unwrap().unwrap();
    assert!(!saved.stale(&cfg, &task));
    assert!(store.events_for_task(task.id, 20).unwrap().iter().any(|e| e.kind == "analysis.recorded"));
    pq(home.path()).args(["task", "analyze", &task.key, "--show"]).assert().success();
    let shown = pq(home.path()).args(["--json", "task", "show", &task.key]).assert().success().get_output().stdout.clone();
    assert_eq!(serde_json::from_slice::<Value>(&shown).unwrap()["analysis"]["proposal"]["project"], "default");
    let mut changed = task.clone();
    changed.description = "Now requires an unknown repository".into();
    store.update_task(&changed).unwrap();
    let shown =
        pq(home.path()).args(["--json", "task", "analyze", &task.key, "--show"]).assert().code(3).get_output().stdout.clone();
    assert_eq!(serde_json::from_slice::<Value>(&shown).unwrap()["stale"], true);
    let doctor = pq(home.path()).args(["--json", "doctor", "--offline"]).output().unwrap();
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("Issue analysis"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_analysis_never_overwrites_a_concurrent_start_or_source_edit() {
    let server = MockServer::start().await;
    Mock::given(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(500)).set_body_json(json!({
            "stop_reason":"end_turn", "content":[{"type":"text", "text":serde_json::to_string(&proposal()).unwrap()}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (home, _, _, store, task) = setup(&format!("{}/messages", server.uri()));
    let root = home.path().to_owned();
    let key = task.key.clone();
    let run = tokio::task::spawn_blocking(move || {
        pq(&root)
            .env("POWERQUEUE_ANALYSIS_API_KEY", "test-only")
            .args(["--json", "task", "analyze", &key])
            .assert()
            .code(3)
            .get_output()
            .stdout
            .clone()
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while server.received_requests().await.unwrap().is_empty() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let mut changed = task.clone();
    changed.state = TaskState::Starting;
    changed.attempts = 1;
    changed.execution_repo = Some("/bound/checkout".into());
    changed.description = "Source changed while the request ran".into();
    store.update_task(&changed).unwrap();
    let expected = store.get_task(task.id).unwrap().unwrap();
    let output = run.await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&output).unwrap()["stale"], true);
    assert_eq!(store.get_task(task.id).unwrap().unwrap(), expected);
    assert!(store.kv_get::<AnalysisRecord>(&record_key(task.id)).unwrap().is_some());
}

#[test]
fn result_and_event_are_atomic_and_older_results_cannot_replace_newer_advice() {
    let (_home, paths, cfg, store, task) = setup("https://example.test/messages");
    let input = AnalysisInput::new(&cfg, &task).unwrap();
    let mut record = AnalysisRecord {
        created_at: chrono::Utc::now(),
        fingerprint: input.fingerprint(&cfg),
        proposal: Some(proposal()),
        attempts: vec![],
    };
    let writer = rusqlite::Connection::open(paths.database()).unwrap();
    writer.execute_batch("CREATE TRIGGER reject_analysis BEFORE INSERT ON events WHEN NEW.kind = 'analysis.recorded' BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
    assert!(store.save_analysis(task.id, &record).is_err());
    assert!(store.kv_get::<AnalysisRecord>(&record_key(task.id)).unwrap().is_none());
    writer.execute_batch("DROP TRIGGER reject_analysis").unwrap();
    assert!(store.save_analysis(task.id, &record).unwrap());
    let newest = record.clone();
    record.created_at -= chrono::Duration::seconds(10);
    assert!(!store.save_analysis(task.id, &record).unwrap());
    assert_eq!(store.kv_get::<AnalysisRecord>(&record_key(task.id)).unwrap(), Some(newest));
    assert_eq!(store.get_task(task.id).unwrap().unwrap(), task);
}

#[test]
fn analysis_is_disabled_by_default_and_missing_credentials_are_diagnostic() {
    let (home, paths, mut cfg, store, task) = setup("https://example.test/messages");
    cfg.analysis.enabled = false;
    cfg.save(&paths).unwrap();
    pq(home.path()).args(["task", "analyze", &task.key]).assert().failure();
    assert!(store.kv_get::<AnalysisRecord>(&record_key(task.id)).unwrap().is_none());
    cfg.analysis.enabled = true;
    cfg.save(&paths).unwrap();
    pq(home.path()).args(["task", "analyze", &task.key]).assert().code(3);
    let record: AnalysisRecord = store.kv_get(&record_key(task.id)).unwrap().unwrap();
    assert!(record.attempts[0].error.as_ref().unwrap().contains("key"));
    assert_eq!(store.get_task(task.id).unwrap().unwrap(), task);
}

#[test]
fn saved_model_controls_are_escaped_even_without_color() {
    let (home, _, cfg, store, task) = setup("https://example.test/messages");
    let input = AnalysisInput::new(&cfg, &task).unwrap();
    let mut proposal = proposal();
    proposal.reasoning = "\u{1b}[2J\u{1b}]52;c;YXBwcm92ZWQ=\u{7} forged approval\r".into();
    proposal.plan = vec!["\u{202e} misleading direction\nnext line".into()];
    let record = AnalysisRecord {
        created_at: chrono::Utc::now(),
        fingerprint: input.fingerprint(&cfg),
        proposal: Some(proposal),
        attempts: vec![],
    };
    store.save_analysis(task.id, &record).unwrap();
    let output = pq(home.path())
        .args(["--no-color", "task", "analyze", &task.key, "--show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    assert!(!output.contains('\u{1b}'));
    assert!(!output.contains('\u{7}'));
    assert!(!output.contains('\r'));
    assert!(!output.contains('\u{202e}'));
    assert!(output.contains("source approval still applies"));
    assert!(output.contains("Source approval and priority still apply"));
}

#[test]
fn automatic_assignment_is_atomic_sticky_and_guarded_against_stale_writers() {
    let (_home, paths, mut cfg, store, mut task) = setup("https://example.test/messages");
    cfg.routing.mode = powerqueue::routing::RoutingMode::Automatic;
    cfg.github.enabled = true;
    cfg.github.repository = "example/issues".into();
    cfg.projects.insert("infra".into(), powerqueue::projects::ProjectConfig::default());
    cfg.projects.get_mut("infra").unwrap().repo.path = "/infra".into();
    cfg.analysis.projects.insert("infra".into(), "Infrastructure".into());
    task = store.get_task(task.id).unwrap().unwrap();
    let mut record = AnalysisRecord {
        created_at: chrono::Utc::now(),
        fingerprint: AnalysisInput::new(&cfg, &task).unwrap().fingerprint(&cfg),
        proposal: Some(Proposal { project: Some("infra".into()), ..proposal() }),
        attempts: vec![],
    };
    store.save_analysis(task.id, &record).unwrap();
    let writer = rusqlite::Connection::open(paths.database()).unwrap();
    writer.execute_batch("CREATE TRIGGER reject_assignment BEFORE INSERT ON events WHEN NEW.kind = 'routing.automatic_assigned' BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
    assert!(store.apply_analysis_project(task.id, &cfg).is_err());
    assert_eq!(store.get_task(task.id).unwrap().unwrap(), task);
    writer.execute_batch("DROP TRIGGER reject_assignment").unwrap();
    assert!(store.apply_analysis_project(task.id, &cfg).unwrap());
    let assigned = store.get_task(task.id).unwrap().unwrap();
    assert_eq!(assigned.execution_project, "infra");
    assert_eq!(assigned.github_admission, task.github_admission);
    assert_eq!(assigned.state, task.state);
    assert!(!assigned.execution_approved());
    assert!(cfg.route(&assigned).permitted);
    store.update_task(&task).unwrap(); // stale rescore cannot undo assignment
    assert_eq!(store.get_task(task.id).unwrap().unwrap().execution_project, "infra");
    assert!(!store.record_routing(&task, &task).unwrap());
    store.set_execution_project(task.id, "infra", "default").unwrap();
    let explicit = store.get_task(task.id).unwrap().unwrap();
    assert_eq!(cfg.route(&explicit).project.as_deref(), Some("default"));
    record.created_at += chrono::Duration::seconds(1);
    store.save_analysis(task.id, &record).unwrap();
    assert!(!store.apply_analysis_project(task.id, &cfg).unwrap());
    store.update_task(&assigned).unwrap();
    assert_eq!(store.get_task(task.id).unwrap().unwrap().project_assignment, explicit.project_assignment);
    assert_eq!((store.list_sessions_for_task(task.id).unwrap()).len(), 0);
}

#[test]
fn manual_add_obeys_routing_mode_and_explicit_cli_selection() {
    let (home, paths, mut cfg, store, _) = setup("https://example.test/messages");
    cfg.routing.mode = powerqueue::routing::RoutingMode::Automatic;
    cfg.save(&paths).unwrap();
    pq(home.path()).args(["add", "Auto", "--key", "AUTO"]).assert().success();
    let auto = store.get_task_by_key("AUTO").unwrap().unwrap();
    assert_eq!(auto.execution_project, "");
    assert!(!auto.execution_approved());
    pq(home.path()).args(["add", "Manual", "--key", "MANUAL", "--project", "default"]).assert().success();
    let manual = store.get_task_by_key("MANUAL").unwrap().unwrap();
    assert!(matches!(manual.project_assignment, Some(powerqueue::routing::ProjectAssignment::Explicit { .. })));
    cfg.routing.mode = powerqueue::routing::RoutingMode::Explicit;
    cfg.save(&paths).unwrap();
    pq(home.path()).args(["add", "Needs a repo"]).assert().failure();
    pq(home.path()).args(["task", "project", "AUTO", "default"]).assert().success();
    assert!(cfg.route(&store.get_task(auto.id).unwrap().unwrap()).permitted);
}

#[cfg(unix)]
#[test]
fn claude_routing_fallback_can_run_from_inside_claude_code() {
    use std::os::unix::fs::PermissionsExt;
    let (home, paths, mut cfg, _store, task) = setup("https://example.test/messages");
    let script = home.path().join("fake-claude.sh");
    let raw = serde_json::json!({"result": serde_json::to_string(&proposal()).unwrap()}).to_string();
    std::fs::write(&script, format!("#!/bin/sh\ntest -z \"$CLAUDECODE\" || exit 1\ncat >/dev/null\nprintf '%s\\n' '{raw}'\n"))
        .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    cfg.analysis.llm = None;
    cfg.analysis.claude_code =
        Some(powerqueue::analysis::ClaudeCodeConfig { binary: script.display().to_string(), ..Default::default() });
    cfg.save(&paths).unwrap();
    pq(home.path()).env("CLAUDECODE", "1").args(["task", "analyze", &task.key]).assert().success();
}

#[tokio::test(flavor = "multi_thread")]
async fn reanalysis_replaces_or_withholds_an_unstarted_automatic_assignment() {
    let server = MockServer::start().await;
    let (home, paths, mut cfg, store, task) = setup(&format!("{}/messages", server.uri()));
    cfg.routing.mode = powerqueue::routing::RoutingMode::Automatic;
    cfg.github.enabled = true;
    cfg.github.repository = "example/issues".into();
    cfg.projects.insert("infra".into(), powerqueue::projects::ProjectConfig::default());
    cfg.projects.get_mut("infra").unwrap().repo.path = "/infra".into();
    cfg.analysis.projects.insert("infra".into(), "Infrastructure".into());
    cfg.save(&paths).unwrap();
    for (destination, confidence, expected, exit) in [
        (Some("default"), 0.95, Some("default"), 0),
        (Some("infra"), 0.95, Some("infra"), 0),
        (Some("infra"), 0.2, None, 3),
        (Some("default"), 0.95, Some("default"), 0),
        (None, 0.95, None, 3),
    ] {
        server.reset().await;
        let proposal = Proposal { project: destination.map(str::to_owned), confidence, ..proposal() };
        Mock::given(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "stop_reason":"end_turn", "content":[{"type":"text", "text":serde_json::to_string(&proposal).unwrap()}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let output = pq(home.path())
            .env("POWERQUEUE_ANALYSIS_API_KEY", "test-only")
            .args(["--json", "task", "analyze", &task.key])
            .assert()
            .code(exit)
            .get_output()
            .stdout
            .clone();
        let result: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(result["repository_assigned"], expected.is_some());
        let current = store.get_task(task.id).unwrap().unwrap();
        assert_eq!(current.execution_project, expected.unwrap_or(""));
        assert_eq!(cfg.route(&current).permitted, expected.is_some());
        assert_eq!(current.github_admission, task.github_admission);
        assert_eq!(current.state, task.state);
        assert!(!current.execution_approved());
    }
    assert!(store.events_for_task(task.id, 50).unwrap().iter().any(|e| e.kind == "routing.automatic_withheld"));
}

#[tokio::test(flavor = "multi_thread")]
async fn analysis_uses_effective_repository_source_overrides() {
    let server = MockServer::start().await;
    Mock::given(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "stop_reason":"end_turn", "content":[{"type":"text", "text":serde_json::to_string(&proposal()).unwrap()}]
        })))
        .mount(&server)
        .await;
    for enabled in [true, false] {
        let (home, paths, mut cfg, store, task) = setup(&format!("{}/messages", server.uri()));
        cfg.routing.mode = powerqueue::routing::RoutingMode::Automatic;
        // Exercise both a source supplied only by the override and an override
        // disabling a globally configured source. Include the multi-project path.
        cfg.github.enabled = !enabled;
        cfg.github.repository = if enabled { String::new() } else { "example/issues".into() };
        cfg.projects.insert("infra".into(), powerqueue::projects::ProjectConfig::default());
        cfg.projects.get_mut("infra").unwrap().repo.path = "/infra".into();
        cfg.save(&paths).unwrap();
        std::fs::write(
            std::path::Path::new(&cfg.repo.path).join(".powerqueue.toml"),
            format!("[github]\nenabled = {enabled}\nrepository = \"example/issues\"\n"),
        )
        .unwrap();
        let output = pq(home.path())
            .env("POWERQUEUE_ANALYSIS_API_KEY", "test-only")
            .args(["--json", "task", "analyze", &task.key])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let result: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(result["repository_assigned"], enabled);
        let current = store.get_task(task.id).unwrap().unwrap();
        assert_eq!(current.project_assignment.is_some(), enabled);
        assert_eq!(current.github_admission, task.github_admission);
    }
}

#[test]
fn replacement_analysis_and_assignment_roll_back_together() {
    let (_home, paths, mut cfg, store, task) = setup("https://example.test/messages");
    cfg.routing.mode = powerqueue::routing::RoutingMode::Automatic;
    cfg.github.enabled = true;
    cfg.github.repository = "example/issues".into();
    let record = AnalysisRecord {
        created_at: chrono::Utc::now(),
        fingerprint: AnalysisInput::new(&cfg, &task).unwrap().fingerprint(&cfg),
        proposal: Some(proposal()),
        attempts: vec![],
    };
    assert_eq!(store.save_and_apply_analysis(task.id, &record, &cfg).unwrap(), (true, true));
    let before = store.get_task(task.id).unwrap().unwrap();
    let replacement =
        AnalysisRecord { created_at: record.created_at + chrono::Duration::seconds(1), proposal: None, ..record.clone() };
    let writer = rusqlite::Connection::open(paths.database()).unwrap();
    writer.execute_batch("CREATE TRIGGER reject_withholding BEFORE INSERT ON events WHEN NEW.kind = 'routing.automatic_withheld' BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
    assert!(store.save_and_apply_analysis(task.id, &replacement, &cfg).is_err());
    assert_eq!(store.get_task(task.id).unwrap().unwrap(), before);
    assert_eq!(store.kv_get::<AnalysisRecord>(&record_key(task.id)).unwrap().unwrap(), record);
    writer.execute_batch("DROP TRIGGER reject_withholding").unwrap();
    assert_eq!(store.save_and_apply_analysis(task.id, &replacement, &cfg).unwrap(), (true, true));
    assert!(!cfg.route(&store.get_task(task.id).unwrap().unwrap()).permitted);
    assert_eq!(store.save_and_apply_analysis(task.id, &record, &cfg).unwrap(), (false, false));
    assert!(!cfg.route(&store.get_task(task.id).unwrap().unwrap()).permitted);
}
