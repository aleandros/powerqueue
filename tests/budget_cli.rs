//! End-to-end checks for the commands owned by the budget/scheduler area that
//! run without tmux, git or the network: `hook`, `budget ...` and `stop`.

use std::path::Path;

use assert_cmd::Command;
use chrono::Utc;
use powerqueue::budget::{CALIBRATION_KEY, Calibration, RATE_LIMITS_KEY, RateLimitState};
use powerqueue::config::Config;
use powerqueue::domain::{DONE_MARKER, ModelTier, Task, TaskSource, TaskState, TokenUsage, UsageRecord};
use powerqueue::paths::Paths;
use powerqueue::store::Store;
use predicates::prelude::*;

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::rooted(dir.path());
        paths.ensure().expect("create layout");
        let mut cfg = Config::default();
        cfg.repo.path = dir.path().join("repo").display().to_string();
        cfg.budget.providers.claude.period_anchor = Some("2026-09-28T00:00:00Z".to_string());
        cfg.save(&paths).expect("save config");
        Self { dir }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn paths(&self) -> Paths {
        Paths::rooted(self.path())
    }
    fn store(&self) -> Store {
        Store::open(&self.paths().database()).expect("open store")
    }
    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("powerqueue").expect("binary");
        c.env("POWERQUEUE_HOME", self.path())
            .env("POWERQUEUE_SECRETS", "file")
            .env_remove("NO_COLOR")
            .env_remove("LINEAR_API_KEY")
            .env_remove("JEV_API_KEY");
        c
    }
}

fn running_task(store: &Store, key: &str) -> Task {
    let mut t = Task::new(key, format!("Title {key}"), TaskSource::Manual);
    t.state = TaskState::Running;
    store.insert_task(&t).expect("insert task");
    t
}

#[test]
fn hook_stores_payload_and_done_marker_completes_task() {
    let home = Home::new();
    let store = home.store();
    let task = running_task(&store, "ENG-1");
    let sid = uuid::Uuid::new_v4();
    let payload = serde_json::json!({ "session_id": sid, "last_assistant_message": format!("{DONE_MARKER} wired it up") });

    home.cmd()
        .args(["hook", "--task", &task.id.short(), "--session", &sid.to_string(), "--event", "Stop"])
        .write_stdin(payload.to_string())
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    let task = store.get_task(task.id).unwrap().unwrap();
    assert_eq!(task.state, TaskState::Completed);
    assert_eq!(task.summary.as_deref(), Some("wired it up"));
    let pending = store.drain_hook_events().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].session_id, Some(sid));
}

#[test]
fn hook_never_fails_even_with_bad_input() {
    let home = Home::new();
    home.cmd()
        .args(["hook", "--task", "nope", "--event", "Stop"])
        .write_stdin("{}")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no task matches"));
    home.cmd()
        .args(["hook", "--task", "nope", "--event", "Bogus"])
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    let store = home.store();
    let task = running_task(&store, "ENG-2");
    home.cmd().args(["hook", "--task", "eng-2", "--event", "SessionStart"]).write_stdin("not json at all").assert().success();
    let pending = store.drain_hook_events().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].task_id, task.id);
    assert_eq!(pending[0].payload, serde_json::json!({}));
}

#[test]
fn budget_show_reports_spend_and_json() {
    let home = Home::new();
    let store = home.store();
    let task = running_task(&store, "ENG-3");
    store
        .record_usage(&UsageRecord {
            session_id: uuid::Uuid::new_v4(),
            task_id: task.id,
            message_id: "msg_1".into(),
            model_id: "claude-opus-5-5".into(),
            tier: ModelTier::opus(),
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 1000,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
            timestamp: Utc::now(),
        })
        .unwrap();

    home.cmd()
        .args(["budget", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("period"))
        .stdout(predicate::str::contains("what would run now"))
        .stdout(predicate::str::contains("critical"));

    let out = home.cmd().args(["--json", "budget", "show"]).assert().success().get_output().stdout.clone();
    let ledger: serde_json::Value = serde_json::from_slice(&out).expect("ledger json");
    // 1000 output tokens = 5000 weighted × opus weight 3.
    assert_eq!(ledger["total_period_weighted"].as_f64(), Some(15_000.0));
    assert_eq!(ledger["period_budget"].as_f64(), Some(80_000_000.0));
}

#[test]
fn budget_set_reset_and_observed_and_clear_limits() {
    let home = Home::new();
    home.cmd()
        .args(["budget", "set-reset", "2026-10-06T07:00:00Z"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2026-10-06 07:00 UTC"));
    let cfg = Config::load(&home.paths()).unwrap();
    assert_eq!(cfg.budget.providers.claude.period_anchor.as_deref(), Some("2026-10-06T07:00:00Z"));
    home.cmd().args(["budget", "set-reset", "in 2d"]).assert().success();
    home.cmd().args(["budget", "set-reset", "whenever"]).assert().failure().stderr(predicate::str::contains("RFC 3339"));

    home.cmd().args(["budget", "set-observed", "43%"]).assert().success().stdout(predicate::str::contains("43%"));
    let store = home.store();
    let cal: Calibration = store.kv_get(CALIBRATION_KEY).unwrap().expect("calibration stored");
    assert!((cal.observed_fraction - 0.43).abs() < 1e-9);
    assert_eq!(cal.measured_fraction, 0.0);
    home.cmd().args(["budget", "set-observed", "300"]).assert().failure();

    let mut limits = RateLimitState::default();
    limits.mark(ModelTier::fable(), Utc::now() + chrono::Duration::hours(1));
    store.kv_set(RATE_LIMITS_KEY, &limits).unwrap();
    home.cmd().args(["budget", "show"]).assert().success().stdout(predicate::str::contains("rate-limited"));
    home.cmd().args(["budget", "clear-limits"]).assert().success();
    assert!(store.kv_get::<RateLimitState>(RATE_LIMITS_KEY).unwrap().is_none());
}

#[test]
fn budget_provider_flags_and_legacy_config_keep_working() {
    let home = Home::new();
    // A pre-multi-provider config.toml: flat [budget] keys and [budget.models.*].
    let legacy = format!(
        "[repo]\npath = \"{}\"\n[budget]\nperiod_anchor = \"2026-09-28T00:00:00Z\"\nperiod_weighted_tokens = 1000000\n\n[budget.models.fable]\nshare = 0.1\n",
        home.path().join("repo").display()
    );
    std::fs::write(home.paths().config_file(), &legacy).unwrap();

    let out = home.cmd().args(["--json", "budget", "show"]).assert().success().get_output().stdout.clone();
    let ledger: serde_json::Value = serde_json::from_slice(&out).expect("ledger json");
    assert_eq!(ledger["provider"], "claude");
    assert_eq!(ledger["period_budget"].as_f64(), Some(1_000_000.0));
    assert_eq!(ledger["period"]["start"], "2026-09-28T00:00:00Z");
    let fable = ledger["tiers"].as_array().unwrap().iter().find(|t| t["tier"] == "fable").expect("fable tier");
    assert_eq!(fable["period_budget"].as_f64(), Some(100_000.0), "legacy [budget.models.fable] share applies");

    home.cmd().args(["config", "validate"]).assert().success().stdout(predicate::str::contains("is valid"));
    home.cmd().args(["doctor", "--offline", "--json"]).assert().stdout(predicate::str::contains("old budget keys"));

    // `config set` on the legacy key rewrites the file into the new shape.
    home.cmd()
        .args(["config", "set", "budget.models.fable.share", "0.2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("budget.providers.claude.models.fable.share = 0.2"))
        .stderr(predicate::str::contains("now lives at"));
    let text = std::fs::read_to_string(home.paths().config_file()).unwrap();
    assert!(!text.contains("[budget.models.fable]"), "{text}");
    assert!(text.contains("[budget.providers.claude.models.fable]"), "{text}");
    let cfg = Config::load(&home.paths()).unwrap();
    assert!(cfg.deprecations().is_empty(), "{:?}", cfg.deprecations());
    assert_eq!(cfg.budget.providers.claude.period_weighted_tokens, 1_000_000);
    assert_eq!(cfg.budget.providers.claude.models[&ModelTier::fable()].share, 0.2);

    // --provider on the write commands; a disabled provider is refused for set-observed.
    home.cmd()
        .args(["budget", "set-reset", "2026-10-06T07:00:00Z", "--provider", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("codex period anchor set"));
    let cfg = Config::load(&home.paths()).unwrap();
    assert_eq!(cfg.budget.providers.codex.period_anchor.as_deref(), Some("2026-10-06T07:00:00Z"));
    assert_eq!(cfg.budget.providers.claude.period_anchor.as_deref(), Some("2026-09-28T00:00:00Z"));
    home.cmd()
        .args(["budget", "set-observed", "20%", "--provider", "codex"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not enabled"));
    home.cmd().args(["budget", "set-observed", "20%", "--provider", "antigravity"]).assert().failure();
    home.cmd().args(["budget", "set-observed", "20%", "--provider", "bard"]).assert().failure();

    let store = home.store();
    let mut limits = RateLimitState::default();
    limits.mark(ModelTier::fable(), Utc::now() + chrono::Duration::hours(1));
    limits.mark(ModelTier::new("gpt-6.1-sol"), Utc::now() + chrono::Duration::hours(1));
    store.kv_set(RATE_LIMITS_KEY, &limits).unwrap();
    home.cmd().args(["budget", "clear-limits", "--provider", "codex"]).assert().success();
    let left: RateLimitState = store.kv_get(RATE_LIMITS_KEY).unwrap().expect("claude cooldown remains");
    assert!(left.exhausted_until.contains_key(&ModelTier::fable()));
    assert_eq!(left.exhausted_until.len(), 1);
    home.cmd().args(["budget", "clear-limits"]).assert().success();
    assert!(store.kv_get::<RateLimitState>(RATE_LIMITS_KEY).unwrap().is_none());
}

#[test]
fn budget_estimate_without_history_and_for_a_task() {
    let home = Home::new();
    let store = home.store();
    let task = running_task(&store, "ENG-4");
    home.cmd()
        .args(["budget", "estimate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 task(s)"))
        .stdout(predicate::str::contains("not enough"));
    home.cmd().args(["budget", "estimate", "ENG-4"]).assert().success().stdout(predicate::str::contains("default (no history)"));
    let out =
        home.cmd().args(["--json", "budget", "estimate", &task.id.to_string()]).assert().success().get_output().stdout.clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["prediction"]["confidence"].as_f64(), Some(0.0));
    home.cmd().args(["budget", "estimate", "missing"]).assert().failure();
}

#[test]
fn stop_without_a_daemon_reports_and_exits_one() {
    let home = Home::new();
    home.cmd().arg("stop").assert().code(1).stderr(predicate::str::contains("no running daemon detected"));
    let store = home.store();
    assert_eq!(store.drain_commands().unwrap().len(), 1, "the shutdown command is still queued");
    store.heartbeat(4242).unwrap();
    home.cmd().arg("stop").assert().success().stdout(predicate::str::contains("shutdown requested"));
}
