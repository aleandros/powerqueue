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
    assert_eq!(result["requires_human_approval"], true);
    assert_eq!(result["analysis"]["proposal"]["project"], "default");
    assert_eq!(store.get_task(task.id).unwrap().unwrap(), task);
    assert!(store.list_sessions_for_task(task.id).unwrap().is_empty());
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
    assert!(output.contains("human approval required"));
    assert!(output.contains("Analysis never grants approval"));
}
