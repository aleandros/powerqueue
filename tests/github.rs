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
    for status in [401, 403, 404, 429, 500] {
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
    command(&["--json", "github", "sync", "--apply"]).assert().success();
    let out = command(&["--json", "github", "sync"]).assert().success().get_output().stdout.clone();
    let repeat: Value = serde_json::from_slice(&out).unwrap();
    assert!(repeat["changes"].as_array().unwrap().is_empty());
    command(&["--json", "task", "show", "acme/app#1"]).assert().success();
}

#[test]
fn github_priority_rules_and_prompt_use_issue_metadata() {
    let task = task(7);
    let rules = powerqueue::priority::PriorityRules::parse("## Scoring\n- +40 if source: github\n").unwrap();
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
