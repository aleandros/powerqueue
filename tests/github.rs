//! GitHub REST intake and queue reconciliation against a private mock server.
use powerqueue::config::{Config, GitHubConfig};
use powerqueue::domain::{Task, TaskSource, TaskState};
use powerqueue::github::client::{GitHubIssue, RateLimited};
use powerqueue::github::sync::task_from_issue;
use powerqueue::github::{GitHubClient, apply_plan, plan_sync};
use powerqueue::store::Store;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn issue(number: u64) -> Value {
    json!({"number": number, "title": format!("Issue {number}"), "body": "Fix the bug",
        "html_url": format!("https://github.com/acme/app/issues/{number}"), "state": "open",
        "issue_dependencies_summary": {"blocked_by":0,"total_blocked_by":0},
        "labels": [{"name": "agent:ready"}], "created_at": "2026-09-01T12:00:00Z", "updated_at": "2026-09-01T12:01:00Z"})
}
fn config(server: &MockServer) -> GitHubConfig {
    GitHubConfig { enabled: true, repository: "acme/app".into(), endpoint: server.uri(), ..Default::default() }
}
fn client(server: &MockServer) -> GitHubClient {
    GitHubClient::new(&server.uri(), "test-token".into()).unwrap()
}
fn evidence(issue: &Value, actor: &str) -> Value {
    json!({"data": {"repository": {"issue": {
        "title": issue["title"], "body": issue["body"], "state": issue["state"].as_str().unwrap().to_uppercase(),
        "updatedAt": issue["updated_at"], "lastEditedAt": null,
        "timelineItems": {"nodes": [{"__typename": "LabeledEvent", "id": "approval-1", "createdAt": "2026-09-01T12:01:00Z",
            "actor": {"login": actor}, "label": {"name": "agent:ready"}}], "pageInfo": {"hasNextPage": false, "endCursor": null}}
    }}}})
}
async fn approval(server: &MockServer, issues: Vec<Value>) {
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"/repos/[^/]+/[^/]+/issues/\d+/dependencies/blocked_by"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .with_priority(100)
        .mount(server)
        .await;
    if issues.is_empty() {
        return;
    }
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            let number = &body["variables"]["number"];
            let issue = issues.iter().find(|i| &i["number"] == number).unwrap();
            ResponseTemplate::new(200).set_body_json(evidence(issue, "maintainer"))
        })
        .mount(server)
        .await;
}
async fn list(server: &MockServer, issues: Vec<Value>) {
    approval(server, issues.clone()).await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issues))
        .mount(server)
        .await;
}
fn task(number: u64) -> Task {
    task_from_issue("acme/app", &serde_json::from_value::<GitHubIssue>(issue(number)).unwrap())
}

#[tokio::test]
async fn paginates_filters_prs_and_labels_and_caps_raw_entries() {
    let server = MockServer::start().await;
    let mut cfg = config(&server);
    cfg.max_issues = 102;
    cfg.assignee = Some("edgar".into());
    let mut first: Vec<_> = (1..=100).map(issue).collect();
    first[0]["pull_request"] = json!({"url": "https://api.github.com/repos/acme/app/pulls/1"});
    first[1]["labels"] = json!([{"name": "agent:ready"}, {"name": "NO-agent"}]);
    for (page, rows) in [("1", first), ("2", vec![issue(101), issue(102), issue(103)])] {
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues"))
            .and(header("authorization", "Bearer test-token"))
            .and(header("x-github-api-version", "2022-11-28"))
            .and(query_param("labels", "agent:ready"))
            .and(query_param("assignee", "edgar"))
            .and(query_param("state", "open"))
            .and(query_param("page", page))
            .respond_with(ResponseTemplate::new(200).set_body_json(rows))
            .expect(1)
            .mount(&server)
            .await;
    }
    let issues = client(&server).fetch_issues(&cfg).await.unwrap();
    assert_eq!(issues.len(), 100);
    assert_eq!(issues.first().unwrap().number, 3);
    assert_eq!(issues.last().unwrap().number, 102);
}

#[tokio::test]
async fn preview_apply_is_idempotent_preserves_other_sources_and_execution_state() {
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    let cfg = config(&server);
    list(&server, vec![issue(1)]).await;
    let manual = Task::new("manual-1", "manual", TaskSource::Manual);
    store.insert_task(&manual).unwrap();
    let plan = plan_sync(&store, &cfg, &client(&server)).await.unwrap();
    assert_eq!(store.list_tasks().unwrap().len(), 1, "preview must not write");
    apply_plan(&store, &plan).unwrap();
    apply_plan(&store, &plan).unwrap();
    let mut saved = store.get_task_by_key("ACME/APP#1").unwrap().unwrap();
    assert_eq!(saved.description, "Fix the bug");
    assert_eq!(saved.source.kind(), "github");
    assert_eq!(serde_json::to_value(&saved.source).unwrap()["kind"], "github");
    assert_eq!(saved.linear_issue_id(), None);
    assert_eq!(saved.slug(), "acme-app-1");
    assert_eq!(store.list_tasks().unwrap().len(), 2);
    assert!(plan_sync(&store, &cfg, &client(&server)).await.unwrap().changes.is_empty());
    server.reset().await;
    let mut updated = issue(1);
    updated["title"] = json!("New title");
    list(&server, vec![updated]).await;
    let plan = plan_sync(&store, &cfg, &client(&server)).await.unwrap();
    saved.state = TaskState::Running;
    saved.attempts = 3;
    store.update_task(&saved).unwrap();
    apply_plan(&store, &plan).unwrap();
    let saved = store.get_task(saved.id).unwrap().unwrap();
    assert_eq!(saved.title, "New title");
    assert_eq!(saved.state, TaskState::Running);
    assert_eq!(saved.attempts, 3);
    assert!(store.events_for_task(saved.id, 10).unwrap().iter().any(|e| e.kind == "task.updated"));
}

#[tokio::test]
async fn only_confirmed_closed_inactive_tasks_are_cancelled() {
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    let cfg = config(&server);
    for (n, state) in [(1, TaskState::Queued), (2, TaskState::Running), (3, TaskState::Paused), (4, TaskState::Completed)] {
        let mut t = task(n);
        t.state = state;
        store.insert_task(&t).unwrap();
    }
    list(&server, vec![]).await;
    for (n, state) in [(1, "closed"), (2, "open"), (3, "open")] {
        let mut remote = issue(n);
        remote["state"] = json!(state);
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/app/issues/{n}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(remote))
            .expect(1)
            .mount(&server)
            .await;
    }
    approval(&server, vec![issue(2), issue(3)]).await;
    let plan = plan_sync(&store, &cfg, &client(&server)).await.unwrap();
    assert_eq!(plan.changes.len(), 3);
    assert!(plan.changes.iter().any(|c| c.action == "cancel"));
    apply_plan(&store, &plan).unwrap();
    assert_eq!(store.get_task_by_key("acme/app#1").unwrap().unwrap().state, TaskState::Cancelled);
    assert_eq!(store.get_task_by_key("acme/app#2").unwrap().unwrap().state, TaskState::Running);
    assert_eq!(store.get_task_by_key("acme/app#3").unwrap().unwrap().state, TaskState::Paused);
}

#[tokio::test]
async fn missing_access_and_server_failures_never_cancel_or_partially_import() {
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    let tracked = task(1);
    store.insert_task(&tracked).unwrap();
    for status in [401, 403, 429, 500] {
        server.reset().await;
        list(&server, vec![issue(2)]).await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues/1"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        assert!(plan_sync(&store, &config(&server), &client(&server)).await.is_err());
        assert_eq!(store.get_task(tracked.id).unwrap().unwrap().state, TaskState::Queued);
        assert_eq!(store.list_tasks().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn lifecycle_only_removes_managed_labels_and_posts_expected_payloads() {
    let server = MockServer::start().await;
    let mut remote = issue(1);
    remote["labels"] = json!([{"name": "agent:ready"}, {"name": "agent:working"}, {"name": "bug"}]);
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/issues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(remote))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/app/issues/1/labels"))
        .and(body_json(json!({"labels": ["agent:review"]})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/repos/acme/app/issues/1/labels/agent:working"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/acme/app/issues/1"))
        .and(body_json(json!({"state": "closed", "state_reason": "completed"})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/app/issues/1/comments"))
        .and(body_json(json!({"body": "Done"})))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    client.set_label("acme/app", 1, "agent:review", &["agent:working", "agent:review", "agent:blocked"]).await.unwrap();
    client.close_issue("acme/app", 1).await.unwrap();
    client.comment("acme/app", 1, "Done").await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 5);
}

#[tokio::test]
async fn rate_limit_reset_is_reported_and_comments_are_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/app/issues/1/comments"))
        .respond_with(ResponseTemplate::new(403).insert_header("retry-after", "120"))
        .expect(1)
        .mount(&server)
        .await;
    let before = chrono::Utc::now();
    let error = client(&server).comment("acme/app", 1, "hello").await.unwrap_err();
    let limit = error.downcast_ref::<RateLimited>().unwrap();
    assert!(limit.retry_at >= before + chrono::Duration::seconds(120));
}

#[test]
fn defaults_and_validation_are_backwards_compatible() {
    let cfg = Config::from_toml("[linear]\nenabled = false\n").unwrap();
    assert!(!cfg.github.enabled);
    assert!(!cfg.github.close_on_complete);
    assert!(GitHubConfig::default().validate().is_empty());
    for repository in ["", "owner", "a/b/c", "a/..", "a/b?x", "a/ b"] {
        let cfg = GitHubConfig { enabled: true, repository: repository.into(), ..Default::default() };
        assert!(!cfg.validate().is_empty(), "{repository}");
    }
    assert!(Config::from_toml("[github]\nenabeld = true").is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_preview_apply_and_json_work_with_file_secrets() {
    let server = MockServer::start().await;
    list(&server, vec![issue(1)]).await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/app"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"full_name": "acme/app"})))
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = format!(
        "[repo]\npath = {:?}\n[linear]\nenabled = false\n[github]\nrepository = 'acme/app'\nendpoint = {:?}\n",
        tmp.path(),
        server.uri()
    );
    std::fs::create_dir_all(tmp.path().join("config")).unwrap();
    std::fs::write(tmp.path().join("config/config.toml"), cfg).unwrap();
    let command = |args: &[&str]| {
        let mut cmd = assert_cmd::Command::cargo_bin("powerqueue").unwrap();
        cmd.env("POWERQUEUE_HOME", tmp.path()).env("POWERQUEUE_SECRETS", "file").env_remove("GITHUB_TOKEN").args(args);
        cmd
    };
    command(&["secrets", "set", "github", "test-token"]).assert().success();
    command(&["--json", "github", "test"]).assert().success();
    let out = command(&["--json", "github", "sync"]).assert().success().get_output().stdout.clone();
    let preview: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["changes"][0]["action"], "create");
    assert_eq!(
        preview["changes"][0]["task"]["execution_project"], "default",
        "disabled source previews keep their configured destination"
    );
    assert_eq!(preview["changes"][0]["task"]["routing"]["permitted"], true);
    command(&["--json", "github", "sync", "--apply"]).assert().success();
    let out = command(&["--json", "github", "sync"]).assert().success().get_output().stdout.clone();
    let repeat: Value = serde_json::from_slice(&out).unwrap();
    assert!(repeat["changes"].as_array().unwrap().is_empty());
    let out = command(&["--json", "task", "show", "acme/app#1"]).assert().success().get_output().stdout.clone();
    let saved: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(saved["task"]["execution_project"], "default");
    assert_eq!(saved["current_routing"]["permitted"], false, "disabled sources still cannot launch");
    assert!(saved["current_routing"]["reason"].as_str().unwrap().contains("disabled"));
}

#[test]
fn github_priority_rules_and_prompt_use_issue_metadata() {
    let task = task(7);
    let rules = powerqueue::priority::PriorityRules::parse("version = 1\ndefault_criticality = \"normal\"\n[[scoring]]\ndelta = 40.0\nall = [{ field = \"source\", op = \"eq\", value = \"github\" }]\n[jev]\nenabled = false\nquestion = \"How important is it to ship this ticket this week?\"\nlevels = [\"can wait indefinitely\", \"nice to have\", \"important this week\", \"blocking customers or revenue\"]\n").unwrap();
    let score = rules.evaluate(&task, task.created_at, None, 0.0, 0.0);
    assert!(score.reasons.iter().any(|r| r.contains("source: github")));
    let prompt =
        powerqueue::session::launcher::build_prompt(&task, &Config::default(), powerqueue::domain::Provider::Claude, 1, None);
    assert!(prompt.contains("acme/app#7"));
    assert!(prompt.contains("https://github.com/acme/app/issues/7"));
    assert!(prompt.contains("Fix the bug"));
}

#[tokio::test]
async fn completed_but_open_issues_do_not_starve_intake() {
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    let mut cfg = config(&server);
    cfg.max_issues = 1;
    for number in 1..=100 {
        let mut t = task(number);
        t.state = TaskState::Completed;
        store.insert_task(&t).unwrap();
    }
    for (page, rows) in [("1", (1..=100).map(issue).collect::<Vec<_>>()), ("2", vec![issue(101)])] {
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues"))
            .and(query_param("page", page))
            .respond_with(ResponseTemplate::new(200).set_body_json(rows))
            .expect(1)
            .mount(&server)
            .await;
    }
    approval(&server, vec![issue(101)]).await;
    let plan = plan_sync(&store, &cfg, &client(&server)).await.unwrap();
    assert_eq!(plan.changes.len(), 1);
    assert_eq!(plan.changes[0].task.key, "acme/app#101");
}

#[tokio::test]
async fn enterprise_api_base_path_and_encoded_label_names_are_preserved() {
    let server = MockServer::start().await;
    let client = GitHubClient::new(&format!("{}/api/v3/", server.uri()), "test-token".into()).unwrap();
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/app"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client.test_repository("acme/app").await.unwrap();
    let mut remote = issue(1);
    remote["labels"] = json!([{"name": "agent/working"}]);
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/app/issues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(remote))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v3/repos/acme/app/issues/1/labels"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v3/repos/acme/app/issues/1/labels/agent%2Fworking"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    client.set_label("acme/app", 1, "done", &["agent/working", "done"]).await.unwrap();
}

#[tokio::test]
async fn primary_rate_limit_reset_is_honored() {
    let server = MockServer::start().await;
    let reset = chrono::Utc::now() + chrono::Duration::hours(1);
    Mock::given(method("GET"))
        .and(path("/repos/acme/app"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset.timestamp().to_string())
                .insert_header("retry-after", "10"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let error = client(&server).test_repository("acme/app").await.unwrap_err();
    assert_eq!(error.downcast_ref::<RateLimited>().unwrap().retry_at.timestamp(), reset.timestamp());
}

async fn evidence_response(server: &MockServer, response: Value) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(server)
        .await;
}

#[tokio::test]
async fn approval_uses_effective_label_actor_and_requires_reapproval_after_material_edits() {
    let server = MockServer::start().await;
    let mut cfg = config(&server);
    cfg.trusted_approvers = vec!["Maintainer".into()];
    let remote = issue(1);
    let parsed = serde_json::from_value(remote.clone()).unwrap();
    let base = evidence(&remote, "maintainer");
    for (name, response, approved) in [
        ("trusted", base.clone(), true),
        ("untrusted", evidence(&remote, "outsider"), false),
        (
            "missing actor",
            {
                let mut r = base.clone();
                r["data"]["repository"]["issue"]["timelineItems"]["nodes"][0]["actor"] = Value::Null;
                r
            },
            false,
        ),
        (
            "body edited",
            {
                let mut r = base.clone();
                r["data"]["repository"]["issue"]["lastEditedAt"] = json!("2026-09-01T12:02:00Z");
                r
            },
            false,
        ),
        (
            "same second edit",
            {
                let mut r = base.clone();
                r["data"]["repository"]["issue"]["lastEditedAt"] = json!("2026-09-01T12:01:00Z");
                r
            },
            false,
        ),
        (
            "title edited",
            {
                let mut r = base.clone();
                r["data"]["repository"]["issue"]["timelineItems"]["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"__typename":"RenamedTitleEvent", "createdAt":"2026-09-01T12:02:00Z"}));
                r
            },
            false,
        ),
        (
            "removed",
            {
                let mut r = base.clone();
                r["data"]["repository"]["issue"]["timelineItems"]["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"__typename":"UnlabeledEvent", "label":{"name":"agent:ready"}}));
                r
            },
            false,
        ),
        (
            "untrusted reapplication",
            {
                let mut r = base.clone();
                let event = evidence(&remote, "outsider")["data"]["repository"]["issue"]["timelineItems"]["nodes"][0].clone();
                r["data"]["repository"]["issue"]["timelineItems"]["nodes"].as_array_mut().unwrap().push(event);
                r
            },
            false,
        ),
        (
            "reapproved after edit",
            {
                let mut r = base.clone();
                r["data"]["repository"]["issue"]["lastEditedAt"] = json!("2026-09-01T12:00:30Z");
                r
            },
            true,
        ),
        (
            "missing event",
            {
                let mut r = base.clone();
                r["data"]["repository"]["issue"]["timelineItems"]["nodes"] = json!([]);
                r
            },
            false,
        ),
    ] {
        server.reset().await;
        evidence_response(&server, response).await;
        assert_eq!(client(&server).admission(&cfg, &parsed).await.unwrap().approved, approved, "{name}");
    }
    cfg.trusted_approvers.clear();
    server.reset().await;
    evidence_response(&server, evidence(&remote, "outsider")).await;
    assert!(client(&server).admission(&cfg, &parsed).await.unwrap().approved);
}

#[tokio::test]
async fn unavailable_or_inconsistent_history_fails_closed() {
    let server = MockServer::start().await;
    let remote = issue(1);
    let parsed = serde_json::from_value(remote.clone()).unwrap();
    for response in [
        json!({"errors":[{"message":"forbidden"}]}),
        json!({"data":{"repository":{"issue":null}}}),
        {
            let mut r = evidence(&remote, "maintainer");
            r["data"]["repository"]["issue"]["title"] = json!("new scope");
            r
        },
        {
            let mut r = evidence(&remote, "maintainer");
            r["data"]["repository"]["issue"].as_object_mut().unwrap().remove("lastEditedAt");
            r
        },
        {
            let mut r = evidence(&remote, "maintainer");
            r["data"]["repository"]["issue"]["updatedAt"] = json!("2026-09-01T12:03:00Z");
            r
        },
        {
            let mut r = evidence(&remote, "maintainer");
            r["data"]["repository"]["issue"]["timelineItems"]["nodes"] = json!([null]);
            r
        },
    ] {
        server.reset().await;
        evidence_response(&server, response).await;
        assert!(client(&server).admission(&config(&server), &parsed).await.is_err());
    }
    for status in [403, 429, 500] {
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;
        assert!(client(&server).admission(&config(&server), &parsed).await.is_err());
    }
}

#[tokio::test]
async fn removed_approval_survives_restart_and_reapplication_restores_eligibility() {
    use powerqueue::scheduler::pick_next;
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let store = Store::open(&database).unwrap();
    let cfg = config(&server);
    list(&server, vec![issue(1)]).await;
    apply_plan(&store, &plan_sync(&store, &cfg, &client(&server)).await.unwrap()).unwrap();
    assert!(pick_next(&store.list_open_tasks().unwrap(), chrono::Utc::now()).is_some());
    server.reset().await;
    let mut removed = issue(1);
    removed["labels"] = json!([{"name":"bug"}, {"name":"planning"}]);
    list(&server, vec![]).await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/issues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(removed))
        .mount(&server)
        .await;
    apply_plan(&store, &plan_sync(&store, &cfg, &client(&server)).await.unwrap()).unwrap();
    drop(store);
    let store = Store::open(&database).unwrap();
    let task = store.get_task_by_key("acme/app#1").unwrap().unwrap();
    assert_eq!(task.state, TaskState::Queued);
    assert!(!task.execution_approved());
    assert!(pick_next(&store.list_open_tasks().unwrap(), chrono::Utc::now()).is_none());
    assert!(store.events_for_task(task.id, 20).unwrap().iter().any(|e| e.kind == "github.revoked"));
    server.reset().await;
    list(&server, vec![issue(1)]).await;
    apply_plan(&store, &plan_sync(&store, &cfg, &client(&server)).await.unwrap()).unwrap();
    assert!(pick_next(&store.list_open_tasks().unwrap(), chrono::Utc::now()).is_some());
}

#[test]
fn approval_labels_remain_configurable_and_legacy_explicit_settings_are_preserved() {
    assert_eq!(GitHubConfig::default().required_labels, ["agent:ready"]);
    for label in ["powerqueue", "team:approved"] {
        let cfg = Config::from_toml(&format!("[github]\nrequired_labels = [{label:?}]\ntrusted_approvers = ['edgar']")).unwrap();
        assert_eq!(cfg.github.required_labels, [label]);
        assert_eq!(cfg.github.trusted_approvers, ["edgar"]);
    }
    for extra in
        ["required_labels = []", "trusted_approvers = ['']", "done_label = 'agent:ready'", "excluded_labels = ['agent:ready']"]
    {
        assert!(
            !Config::from_toml(&format!("[github]\nenabled = true\nrepository = 'acme/app'\n{extra}"))
                .unwrap()
                .github
                .validate()
                .is_empty(),
            "{extra}"
        );
    }
}

#[tokio::test]
async fn approval_history_is_paginated_and_custom_labels_are_enforced() {
    use wiremock::matchers::body_partial_json;
    let server = MockServer::start().await;
    let mut cfg = config(&server);
    cfg.required_labels = vec!["team:approved".into()];
    cfg.trusted_approvers = vec!["maintainer".into()];
    let mut remote = issue(1);
    remote["labels"] = json!([{"name":"team:approved"}]);
    let parsed = serde_json::from_value(remote.clone()).unwrap();
    let mut first = evidence(&remote, "maintainer");
    first["data"]["repository"]["issue"]["timelineItems"]["nodes"][0]["label"]["name"] = json!("team:approved");
    first["data"]["repository"]["issue"]["timelineItems"]["pageInfo"] = json!({"hasNextPage":true,"endCursor":"page-two"});
    let mut second = evidence(&remote, "outsider");
    second["data"]["repository"]["issue"]["timelineItems"]["nodes"][0]["label"]["name"] = json!("team:approved");
    for (cursor, response) in [(Value::Null, first), (json!("page-two"), second)] {
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_partial_json(json!({"variables":{"after":cursor}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&server)
            .await;
    }
    let decision = client(&server).admission(&cfg, &parsed).await.unwrap();
    assert!(!decision.approved);
    assert!(decision.reason.contains("untrusted actor outsider"));
}

#[tokio::test]
async fn active_revocation_keeps_work_and_edits_during_downtime_require_reapproval() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let cfg = config(&server);
    let store = Store::open(&database).unwrap();
    list(&server, vec![issue(1)]).await;
    apply_plan(&store, &plan_sync(&store, &cfg, &client(&server)).await.unwrap()).unwrap();
    let mut running = store.get_task_by_key("acme/app#1").unwrap().unwrap();
    running.state = TaskState::Running;
    running.branch = Some("pq/work".into());
    running.worktree_path = Some("/work".into());
    store.update_task(&running).unwrap();
    drop(store);
    server.reset().await;
    let store = Store::open(&database).unwrap();
    let mut remote = issue(1);
    remote["body"] = json!("Entirely new requested work");
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![remote.clone()]))
        .mount(&server)
        .await;
    let mut response = evidence(&remote, "maintainer");
    response["data"]["repository"]["issue"]["lastEditedAt"] = json!("2026-09-01T12:01:00Z");
    evidence_response(&server, response).await;
    apply_plan(&store, &plan_sync(&store, &cfg, &client(&server)).await.unwrap()).unwrap();
    let revoked = store.get_task(running.id).unwrap().unwrap();
    assert_eq!(revoked.state, TaskState::Running);
    assert_eq!(revoked.branch, running.branch);
    assert_eq!(revoked.worktree_path, running.worktree_path);
    assert!(!revoked.execution_approved());
    assert!(revoked.github_admission.unwrap().reason.contains("title or body edited"));
}

#[test]
fn concurrent_revocation_or_scope_change_prevents_claiming_a_cached_start() {
    use powerqueue::domain::GitHubAdmission;
    let store = Store::open_in_memory().unwrap();
    let mut candidate = task(1);
    candidate.github_admission = Some(GitHubAdmission { approved: true, reason: "trusted event".into() });
    store.insert_task(&candidate).unwrap();
    let mut starting = candidate.clone();
    starting.state = TaskState::Starting;
    for mutation in ["revoke", "edit", "pause"] {
        store.update_task(&candidate).unwrap();
        let mut changed = candidate.clone();
        match mutation {
            "revoke" => changed.github_admission = Some(GitHubAdmission::denied("label removed")),
            "edit" => changed.description = "New scope".into(),
            "pause" => changed.state = TaskState::Paused,
            _ => unreachable!(),
        }
        store.update_task(&changed).unwrap();
        assert!(!store.record_github_admission(&candidate, &candidate).unwrap());
        assert!(!store.claim_task_start(&candidate, &starting).unwrap());
        let saved = store.get_task(candidate.id).unwrap().unwrap();
        assert_eq!(saved.state, changed.state);
        assert_eq!(saved.github_admission, changed.github_admission);
    }
}

#[test]
fn schema_upgrade_leaves_legacy_github_tasks_unverified() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let store = Store::open(&database).unwrap();
    let legacy = task(1);
    store.insert_task(&legacy).unwrap();
    drop(store);
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection.execute("ALTER TABLE tasks DROP COLUMN github_admission", []).unwrap();
    connection.pragma_update(None, "user_version", 7).unwrap();
    drop(connection);
    let store = Store::open(&database).unwrap();
    let restored = store.get_task(legacy.id).unwrap().unwrap();
    assert_eq!(restored.state, TaskState::Queued);
    assert!(restored.github_admission.is_none());
    assert!(!restored.execution_approved());
}

#[test]
fn source_registry_migrates_legacy_and_rejects_unknown_or_conflicting_routes() {
    let mut cfg =
        Config::from_toml("[repo]\npath = '/tmp/default'\n[github]\nenabled = true\nrepository = 'acme/app'\n").unwrap();
    assert!(cfg.github_routing_problems().is_empty());
    assert_eq!(cfg.github_source("ACME/APP").unwrap().execution_project, "default");
    assert_eq!(cfg.github_sources().len(), 1);
    cfg.github_sources.push(GitHubConfig {
        enabled: true,
        repository: "ACME/App".into(),
        execution_project: "api".into(),
        ..Default::default()
    });
    assert!(cfg.github_source("acme/app").is_err());
    assert_eq!(cfg.github_routing_problems().len(), 2);
    cfg.github_sources[0].repository = "acme/api".into();
    let mut project = powerqueue::projects::ProjectConfig::default();
    project.repo.path = "/tmp/api".into();
    cfg.projects.insert("api".into(), project);
    assert!(cfg.github_routing_problems().is_empty());
    let roundtrip = Config::from_toml(&toml::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(roundtrip, cfg);
    let dir = tempfile::tempdir().unwrap();
    assert!(cfg.for_project("api", &powerqueue::paths::Paths::rooted(dir.path())).is_ok());
}

#[tokio::test]
async fn multi_source_identity_routing_and_reconciliation_remain_isolated() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    let mut ca = config(&a);
    ca.execution_project = "frontend".into();
    let mut cb = config(&b);
    cb.repository = "acme/backend".into();
    cb.execution_project = "backend".into();
    list(&a, vec![issue(1)]).await;
    let mut other = issue(1);
    other["html_url"] = json!("https://github.com/acme/backend/issues/1");
    approval(&b, vec![other.clone()]).await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/backend/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![other]))
        .mount(&b)
        .await;
    for (source, server) in [(&ca, &a), (&cb, &b)] {
        let plan = plan_sync(&store, source, &client(server)).await.unwrap();
        assert_eq!(plan.changes[0].task.execution_project, source.execution_project);
        apply_plan(&store, &plan).unwrap();
        assert!(plan_sync(&store, source, &client(server)).await.unwrap().changes.is_empty());
    }
    assert_eq!(store.list_tasks().unwrap().len(), 2);
    // Metadata and mapping changes cannot silently retarget queued or active work.
    ca.execution_project = "backend".into();
    let plan = plan_sync(&store, &ca, &client(&a)).await.unwrap();
    assert_eq!(plan.changes[0].task.execution_project, "frontend");
    assert!(!plan.changes[0].task.execution_approved());
    let mut active = store.get_task_by_key("acme/app#1").unwrap().unwrap();
    active.state = TaskState::Running;
    active.attempts = 2;
    active.execution_repo = Some("/original/checkout".into());
    store.update_task(&active).unwrap();
    apply_plan(&store, &plan).unwrap();
    let saved = store.get_task(active.id).unwrap().unwrap();
    assert_eq!(saved.state, TaskState::Running);
    assert_eq!(saved.execution_project, "frontend");
    assert_eq!(saved.execution_repo, active.execution_repo);
    assert!(!saved.execution_approved());
    // Failed access to A cannot close B; only B's confirmed closure can do that.
    a.reset().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(404)).mount(&a).await;
    assert!(plan_sync(&store, &ca, &client(&a)).await.is_err());
    assert_eq!(store.get_task_by_key("acme/backend#1").unwrap().unwrap().state, TaskState::Queued);
    b.reset().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/backend/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&b)
        .await;
    let mut closed = issue(1);
    closed["state"] = json!("closed");
    Mock::given(method("GET"))
        .and(path("/repos/acme/backend/issues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(closed))
        .mount(&b)
        .await;
    apply_plan(&store, &plan_sync(&store, &cb, &client(&b)).await.unwrap()).unwrap();
    assert_eq!(store.get_task_by_key("acme/backend#1").unwrap().unwrap().state, TaskState::Cancelled);
    assert_eq!(store.get_task(active.id).unwrap().unwrap(), saved);
}

#[tokio::test]
async fn stale_preview_cannot_approve_a_concurrently_reassigned_task() {
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    let mut saved = task(1);
    saved.github_admission = None;
    store.insert_task(&saved).unwrap();
    list(&server, vec![issue(1)]).await;
    let plan = plan_sync(&store, &config(&server), &client(&server)).await.unwrap();
    store.set_execution_project(saved.id, "default", "other").unwrap();
    apply_plan(&store, &plan).unwrap();
    saved = store.get_task(saved.id).unwrap().unwrap();
    assert_eq!(saved.execution_project, "other");
    assert!(!saved.execution_approved());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_multi_source_tokens_partial_failure_preview_and_manual_selection() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    list(&a, vec![issue(1)]).await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/backend/issues"))
        .and(header("authorization", "Bearer second-token"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&b)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = format!(
        r#"
[repo]
path = {root:?}
[linear]
enabled = false
[projects.api.repo]
path = {root:?}
[[github_sources]]
enabled = true
repository = "acme/app"
execution_project = "api"
endpoint = {a:?}
token_env = "PQ_TEST_SOURCE_A_TOKEN"
[[github_sources]]
enabled = true
repository = "acme/backend"
execution_project = "default"
endpoint = {b:?}
token_env = "PQ_TEST_SOURCE_B_TOKEN"
"#,
        root = tmp.path(),
        a = a.uri(),
        b = b.uri()
    );
    std::fs::create_dir_all(tmp.path().join("config")).unwrap();
    std::fs::write(tmp.path().join("config/config.toml"), cfg).unwrap();
    let command = |args: &[&str]| {
        let mut cmd = assert_cmd::Command::cargo_bin("powerqueue").unwrap();
        cmd.env("POWERQUEUE_HOME", tmp.path())
            .env("POWERQUEUE_SECRETS", "file")
            .env("GITHUB_TOKEN", "wrong-global-token")
            .env("PQ_TEST_SOURCE_A_TOKEN", "first-token")
            .env("PQ_TEST_SOURCE_B_TOKEN", "second-token")
            .args(args);
        cmd
    };
    let out = command(&["--json", "github", "sync"]).assert().code(1).get_output().stdout.clone();
    let preview: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(preview["changes"][0]["task"]["execution_project"], "api");
    assert_eq!(preview["errors"][0]["repository"], "acme/backend");
    command(&["--json", "task", "show", "acme/app#1"]).assert().failure();
    command(&["--json", "github", "sync", "--apply"]).assert().code(1);
    let out = command(&["--json", "task", "show", "acme/app#1"]).assert().success().get_output().stdout.clone();
    let saved: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(saved["task"]["execution_project"], "api");
    assert_eq!(saved["source_route"]["execution_project"], "api");
    assert_eq!(saved["source_route"]["matches_saved_project"], true);
    for (args, project) in
        [(vec!["--json", "add", "Default manual"], "default"), (vec!["--json", "add", "API manual", "--project", "api"], "api")]
    {
        let out = command(&args).assert().success().get_output().stdout.clone();
        assert_eq!(serde_json::from_slice::<Value>(&out).unwrap()["execution_project"], project);
    }
    command(&["add", "Unknown", "--project", "missing"]).assert().failure();
    let before = a.received_requests().await.unwrap().len();
    let out =
        command(&["--json", "github", "sync"]).env_remove("PQ_TEST_SOURCE_A_TOKEN").assert().code(1).get_output().stdout.clone();
    let errors: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(errors["errors"].as_array().unwrap().len(), 2);
    assert_eq!(a.received_requests().await.unwrap().len(), before, "explicit missing token must not use the global token");
    for request in a.received_requests().await.unwrap() {
        assert_eq!(request.headers.get("authorization").unwrap(), "Bearer first-token");
    }
}

#[tokio::test]
async fn conflicting_source_identity_is_visible_in_preview_and_apply() {
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    list(&server, vec![issue(1)]).await;
    let plan = plan_sync(&store, &config(&server), &client(&server)).await.unwrap();
    let manual = Task::new("acme/app#1", "unrelated manual task", TaskSource::Manual);
    store.insert_task(&manual).unwrap();
    let manual = store.get_task(manual.id).unwrap().unwrap();
    apply_plan(&store, &plan).unwrap();
    let plan = plan_sync(&store, &config(&server), &client(&server)).await.unwrap();
    assert!(plan.changes.is_empty());
    assert!(plan.problems[0].reason.contains("conflicts"));
    assert_eq!(store.get_task(manual.id).unwrap().unwrap(), manual);
}

fn routed_config(server: &MockServer) -> Config {
    let mut cfg: Config = toml::from_str(
        r#"
        [repo]
        path = "/repos/default"
        [projects.api.repo]
        path = "/repos/api"
        [projects.web.repo]
        path = "/repos/web"
        [[routing.rules]]
        name = "api label"
        project = "api"
        all = [{field = "repository", op = "eq", value = "acme/app"}, {field = "label", op = "eq", value = "repo/api"}]
        [[routing.rules]]
        name = "web label"
        project = "web"
        all = [{field = "label", op = "eq", value = "repo/web"}]
    "#,
    )
    .unwrap();
    cfg.github = config(server);
    cfg
}

#[tokio::test]
async fn deterministic_routing_preview_conflicts_and_restart_preserve_projects() {
    use powerqueue::github::plan_sync_routed;
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.sqlite");
    let store = Store::open(&path).unwrap();
    let cfg = routed_config(&server);
    let mut api = issue(1);
    api["labels"] = json!([{"name":"agent:ready"},{"name":"repo/api"}]);
    let mut web = issue(2);
    web["labels"] = json!([{"name":"agent:ready"},{"name":"repo/web"}]);
    let mut conflict = issue(3);
    conflict["labels"] = json!([{"name":"agent:ready"},{"name":"repo/api"},{"name":"repo/web"}]);
    list(&server, vec![api.clone(), web.clone(), conflict.clone()]).await;
    let preview = plan_sync_routed(&store, &cfg.github, &client(&server), &cfg).await.unwrap();
    assert!(store.list_tasks().unwrap().is_empty());
    assert_eq!(preview.changes[0].task.execution_project, "api");
    assert_eq!(preview.changes[1].task.execution_project, "web");
    assert_eq!(preview.changes[2].task.execution_project, "");
    assert!(!preview.changes[2].task.execution_approved());
    assert!(preview.changes[2].task.routing.as_ref().unwrap().reason.contains("conflicting"));
    apply_plan(&store, &preview).unwrap();
    assert!(plan_sync_routed(&store, &cfg.github, &client(&server), &cfg).await.unwrap().changes.is_empty());
    let mut started = store.get_task_by_key("acme/app#1").unwrap().unwrap();
    started.state = TaskState::InReview;
    started.attempts = 1;
    started.execution_repo = Some("/repos/api".into());
    started.branch = Some("pq/api-1".into());
    started.pr_url = Some("https://github.com/acme/api/pull/9".into());
    store.update_task(&started).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    api["labels"] = web["labels"].clone();
    server.reset().await;
    list(&server, vec![api, web, conflict]).await;
    let plan = plan_sync_routed(&store, &cfg.github, &client(&server), &cfg).await.unwrap();
    apply_plan(&store, &plan).unwrap();
    let saved = store.get_task(started.id).unwrap().unwrap();
    assert_eq!(saved.execution_project, "api");
    assert_eq!(saved.execution_repo, started.execution_repo);
    assert_eq!(saved.branch, started.branch);
    assert_eq!(saved.state, TaskState::InReview);
    assert_eq!(saved.pr_url, started.pr_url);
    assert!(saved.github_admission.as_ref().unwrap().approved, "approval remains distinct from routing");
    assert!(!saved.execution_approved());
    assert!(saved.routing.as_ref().unwrap().explanation().contains("retains `api`"));
    assert!(plan_sync_routed(&store, &cfg.github, &client(&server), &cfg).await.unwrap().changes.is_empty());
    assert!(store.events_for_task(saved.id, 100).unwrap().iter().any(|e| e.kind == "routing.blocked"));
    assert!(store.set_execution_project(saved.id, "api", "web").is_err());
    let other = store.get_task_by_key("acme/app#2").unwrap().unwrap();
    assert_eq!(other.execution_project, "web");
    assert!(other.execution_approved());
}

#[tokio::test]
async fn routing_unknown_destination_and_stale_preview_never_reassign() {
    use powerqueue::github::plan_sync_routed;
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    let mut cfg = routed_config(&server);
    let mut row = issue(1);
    row["labels"] = json!([{"name":"agent:ready"},{"name":"repo/api"}]);
    list(&server, vec![row.clone()]).await;
    cfg.routing.rules[0].project = "unknown".into();
    let preview = plan_sync_routed(&store, &cfg.github, &client(&server), &cfg).await.unwrap();
    assert!(!preview.changes[0].task.execution_approved());
    assert!(preview.changes[0].task.routing.as_ref().unwrap().reason.contains("unknown execution project"));
    cfg.routing.rules[0].project = "api".into();
    let preview = plan_sync_routed(&store, &cfg.github, &client(&server), &cfg).await.unwrap();
    apply_plan(&store, &preview).unwrap();
    let saved = store.get_task_by_key("acme/app#1").unwrap().unwrap();
    row["labels"] = json!([{"name":"agent:ready"},{"name":"repo/web"}]);
    server.reset().await;
    list(&server, vec![row]).await;
    let preview = plan_sync_routed(&store, &cfg.github, &client(&server), &cfg).await.unwrap();
    let mut running = saved.clone();
    running.state = TaskState::Running;
    running.attempts = 1;
    running.execution_repo = Some("/repos/api".into());
    store.update_task(&running).unwrap();
    apply_plan(&store, &preview).unwrap();
    let saved = store.get_task(saved.id).unwrap().unwrap();
    assert_eq!(saved.state, TaskState::Running);
    assert_eq!(saved.execution_project, "api");
    assert_eq!(saved.execution_repo, running.execution_repo);
    assert!(!saved.execution_approved());
    assert!(!store.update_source_task(&running, &running).unwrap(), "stale metadata cannot overwrite routing evidence");
}

#[tokio::test]
async fn one_inaccessible_issue_or_collision_does_not_block_healthy_intake() {
    for status in [404, 410] {
        let server = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        let mut inaccessible = task(1);
        inaccessible.github_admission =
            Some(powerqueue::domain::GitHubAdmission { approved: true, reason: "previously approved".into() });
        store.insert_task(&inaccessible).unwrap();
        let manual = Task::new("acme/app#2", "manual", TaskSource::Manual);
        store.insert_task(&manual).unwrap();
        list(&server, vec![issue(2), issue(3)]).await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues/1"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let plan = plan_sync(&store, &config(&server), &client(&server)).await.unwrap();
        assert_eq!(plan.problems.len(), 2);
        apply_plan(&store, &plan).unwrap();
        assert!(store.get_task_by_key("acme/app#3").unwrap().unwrap().execution_approved());
        let held = store.get_task(inaccessible.id).unwrap().unwrap();
        assert_eq!(held.state, TaskState::Queued);
        assert!(!held.execution_approved());
        assert_eq!(store.get_task(manual.id).unwrap().unwrap(), store.get_task_by_key("acme/app#2").unwrap().unwrap());
    }
}

#[tokio::test]
async fn inconsistent_issue_snapshot_denies_only_that_issue() {
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue(1), issue(2)]))
        .mount(&server)
        .await;
    let mut mismatched = issue(1);
    mismatched["updated_at"] = json!("2026-09-01T12:02:00Z");
    approval(&server, vec![mismatched, issue(2)]).await;
    let plan = plan_sync(&store, &config(&server), &client(&server)).await.unwrap();
    assert_eq!(plan.problems.len(), 1);
    apply_plan(&store, &plan).unwrap();
    assert!(!store.get_task_by_key("acme/app#1").unwrap().unwrap().execution_approved());
    assert!(store.get_task_by_key("acme/app#2").unwrap().unwrap().execution_approved());
}

#[tokio::test]
async fn graphql_source_errors_abort_the_plan_and_preserve_rate_reset() {
    use powerqueue::github::client::GraphQlFailure;
    for kind in ["RATE_LIMITED", "SECONDARY_LIMIT", "FORBIDDEN", "INSUFFICIENT_SCOPES"] {
        let server = MockServer::start().await;
        let store = Store::open_in_memory().unwrap();
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue(1), issue(2)]))
            .mount(&server)
            .await;
        let reset = chrono::Utc::now() + chrono::Duration::minutes(10);
        let mut response = ResponseTemplate::new(200).set_body_json(json!({"errors":[{"type":kind}]}));
        if kind == "RATE_LIMITED" {
            response = response
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset.timestamp().to_string());
        }
        if kind == "SECONDARY_LIMIT" {
            response = response.insert_header("retry-after", "600");
        }
        Mock::given(method("POST")).and(path("/graphql")).respond_with(response).expect(1).mount(&server).await;
        let error = plan_sync(&store, &config(&server), &client(&server)).await.unwrap_err();
        if kind == "RATE_LIMITED" {
            assert_eq!(error.downcast_ref::<RateLimited>().unwrap().retry_at.timestamp(), reset.timestamp());
        } else if kind == "SECONDARY_LIMIT" {
            assert!(error.downcast_ref::<RateLimited>().unwrap().retry_at >= reset);
        } else {
            assert!(error.is::<GraphQlFailure>());
        }
        assert!(store.list_tasks().unwrap().is_empty());
    }
}

#[test]
fn repeated_issue_problems_are_logged_once_until_recovery() {
    use powerqueue::github::sync::{Change, SyncPlan, SyncProblem};
    let store = Store::open_in_memory().unwrap();
    let after = chrono::Utc::now() - chrono::Duration::seconds(1);
    let bad =
        SyncPlan { problems: vec![SyncProblem { key: "acme/app#1".into(), reason: "unavailable".into() }], ..Default::default() };
    for _ in 0..3 {
        apply_plan(&store, &bad).unwrap();
    }
    assert_eq!(store.count_events_of_kind("github.issue_failed", after).unwrap(), 1);
    let good = SyncPlan { changes: vec![Change { action: "create", task: task(1) }], ..Default::default() };
    apply_plan(&store, &good).unwrap();
    apply_plan(&store, &bad).unwrap();
    assert_eq!(store.count_events_of_kind("github.issue_failed", after).unwrap(), 2);
}

#[tokio::test]
async fn deferral_survives_source_preview_apply_and_edits() {
    use chrono::{Duration, Utc};
    use powerqueue::deferral::Deferral;
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    let cfg = config(&server);
    let mut remote = issue(1);
    remote["body"] = json!("Powerqueue-Defer-Until: 2099-10-20T09:00:00-06:00");
    list(&server, vec![remote.clone()]).await;
    apply_plan(&store, &plan_sync(&store, &cfg, &client(&server)).await.unwrap()).unwrap();
    let task = store.get_task_by_key("acme/app#1").unwrap().unwrap();
    assert!(!task.deferral_allows(Utc::now()));
    let local = Deferral { until: Utc::now() + Duration::hours(2), reason: Some("local hold".into()) };
    for body in ["Powerqueue-Defer-Until: invalid", "Powerqueue-Defer-Until: 2000-01-01T00:00:00Z", ""] {
        server.reset().await;
        remote["body"] = json!(body);
        list(&server, vec![remote.clone()]).await;
        let preview = plan_sync(&store, &cfg, &client(&server)).await.unwrap();
        // A CLI edit after preview must survive applying the stale remote snapshot.
        store.set_task_deferral(task.id, Some(local.clone())).unwrap();
        apply_plan(&store, &preview).unwrap();
        let saved = store.get_task(task.id).unwrap().unwrap();
        assert_eq!(saved.deferral, Some(local.clone()));
        assert_eq!(saved.source_deferral().is_err(), body.ends_with("invalid"));
        assert!(!saved.deferral_allows(Utc::now()));
    }
    assert!(store.set_task_deferral(task.id, None).unwrap().deferral_allows(Utc::now()));
    assert_eq!(
        store.events_for_task(task.id, 100).unwrap().iter().filter(|e| e.kind == "task.source_deferral_changed").count(),
        3
    );
}
