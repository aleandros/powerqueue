//! Native GitHub prerequisite intake, failure isolation and persistence races.
use chrono::Utc;
use powerqueue::config::GitHubConfig;
use powerqueue::domain::{GitHubAdmission, TaskState};
use powerqueue::github::client::GitHubIssue;
use powerqueue::github::{GitHubClient, apply_plan, plan_sync};
use powerqueue::scheduler::transitions::{on_github_dependencies, on_github_launch_held};
use powerqueue::store::Store;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn issue(open: usize, total: usize) -> Value {
    json!({"number":1,"title":"Dependent work","body":"Implement after prerequisites",
        "html_url":"https://github.com/acme/app/issues/1","state":"open",
        "labels":[{"name":"agent:ready"}],"created_at":"2026-09-01T12:00:00Z","updated_at":"2026-09-01T12:01:00Z",
        "issue_dependencies_summary":{"blocked_by":open,"total_blocked_by":total}})
}
fn dependency(server: &MockServer, repo: &str, number: u64, state: &str) -> Value {
    json!({"number":number,"title":"Prerequisite", "repository_url":format!("{}/repos/{repo}",server.uri()),"state":state})
}
fn config(server: &MockServer) -> GitHubConfig {
    GitHubConfig { enabled: true, repository: "acme/app".into(), endpoint: server.uri(), ..Default::default() }
}
#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
)]
fn client(server: &MockServer) -> GitHubClient {
    GitHubClient::new(&server.uri(), "token".into()).unwrap()
}
async fn snapshot(server: &MockServer, issue: Value, dependencies: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([issue.clone()])))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{"repository":{"issue":{
            "title":issue["title"],"body":issue["body"],"state":"OPEN","updatedAt":issue["updated_at"],"lastEditedAt":null,
            "timelineItems":{"nodes":[{"__typename":"LabeledEvent","id":"approval","createdAt":"2026-09-01T12:01:00Z",
                "actor":{"login":"maintainer"},"label":{"name":"agent:ready"}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}
        }}}})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/issues/1/dependencies/blocked_by"))
        .respond_with(dependencies)
        .mount(server)
        .await;
}

#[tokio::test]
async fn blockers_close_reopen_or_are_removed_without_losing_manual_pause() {
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    let cfg = config(&server);
    for (open, total, state, expected) in [
        (1, 1, "open", TaskState::Blocked),
        (0, 1, "closed", TaskState::Queued),
        (1, 1, "open", TaskState::Blocked),
        (0, 0, "", TaskState::Queued),
    ] {
        server.reset().await;
        let rows = if total == 0 { vec![] } else { vec![dependency(&server, "acme/core", 9, state)] };
        snapshot(&server, issue(open, total), ResponseTemplate::new(200).set_body_json(rows)).await;
        let plan = plan_sync(&store, &cfg, &client(&server)).await.unwrap();
        assert!(plan.problems.is_empty());
        apply_plan(&store, &plan).unwrap();
        let task = store.get_task_by_key("acme/app#1").unwrap().unwrap();
        assert_eq!(task.state, expected);
        let events = store.events_for_task(task.id, 100).unwrap();
        let created = events.iter().find(|e| e.kind == "task.created").unwrap();
        let first_block = events.iter().filter(|e| e.kind == "task.blocked").min_by_key(|e| e.id).unwrap();
        assert!(created.id < first_block.id);
        assert_eq!(first_block.data["previous_state"], "queued");
        if let Some(first_unblock) = events.iter().filter(|e| e.kind == "task.unblocked").min_by_key(|e| e.id) {
            assert!(first_block.id < first_unblock.id);
        }
        assert!(task.execution_approved(), "approval is independent of blockers");
        assert_eq!(task.waiting_on(), if open > 0 { vec!["acme/core#9"] } else { vec![] });
        assert!(plan_sync(&store, &cfg, &client(&server)).await.unwrap().changes.is_empty());
    }
    let mut task = store.get_task_by_key("acme/app#1").unwrap().unwrap();
    task.state = TaskState::Paused;
    store.update_task(&task).unwrap();
    server.reset().await;
    snapshot(&server, issue(1, 1), ResponseTemplate::new(200).set_body_json(vec![dependency(&server, "acme/core", 9, "open")]))
        .await;
    apply_plan(&store, &plan_sync(&store, &cfg, &client(&server)).await.unwrap()).unwrap();
    let saved = store.get_task(task.id).unwrap().unwrap();
    assert_eq!(saved.state, TaskState::Paused);
    assert!(saved.is_waiting());
    let events = store.events_for_task(task.id, 100).unwrap();
    assert!(events.iter().any(|e| e.kind == "github.dependencies_changed"));
    assert!(events.iter().any(|e| e.kind == "task.unblocked"));
    assert!(events.iter().any(|e| e.kind == "task.blocked" && e.data["previous_state"] == "queued"));
}

#[tokio::test]
async fn unavailable_or_incomplete_dependencies_never_release_cached_blockers() {
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    let cfg = config(&server);
    snapshot(&server, issue(1, 1), ResponseTemplate::new(200).set_body_json(vec![dependency(&server, "acme/core", 9, "open")]))
        .await;
    apply_plan(&store, &plan_sync(&store, &cfg, &client(&server)).await.unwrap()).unwrap();
    for response in [ResponseTemplate::new(404), ResponseTemplate::new(200).set_body_json(json!([]))] {
        server.reset().await;
        snapshot(&server, issue(1, 1), response).await;
        let plan = plan_sync(&store, &cfg, &client(&server)).await.unwrap();
        assert_eq!(plan.problems.len(), 1);
        apply_plan(&store, &plan).unwrap();
        let task = store.get_task_by_key("acme/app#1").unwrap().unwrap();
        assert!(!task.execution_approved());
        assert_eq!(task.waiting_on(), vec!["acme/core#9"]);
        assert_eq!(task.state, TaskState::Blocked);
    }
    server.reset().await;
    snapshot(&server, issue(0, 0), ResponseTemplate::new(200).set_body_json(json!([]))).await;
    apply_plan(&store, &plan_sync(&store, &cfg, &client(&server)).await.unwrap()).unwrap();
    let task = store.get_task_by_key("acme/app#1").unwrap().unwrap();
    assert!(task.execution_approved());
    assert_eq!(task.state, TaskState::Queued);
    assert!(store.kv_get::<String>("github.sync_problem.acme/app#1").unwrap().is_none());
}

#[tokio::test]
async fn paginated_dependencies_are_complete_and_never_follow_response_hosts() {
    let server = MockServer::start().await;
    let first: Vec<_> = (1..=100).map(|n| dependency(&server, "acme/core", n, "closed")).collect();
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/issues/1/dependencies/blocked_by"))
        .and(query_param("page", "1"))
        .and(query_param("per_page", "100"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Link", "<https://untrusted.invalid/?page=2>; rel=\"next\"")
                .set_body_json(first),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/issues/1/dependencies/blocked_by"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![dependency(&server, "acme/other", 1, "open")]))
        .expect(1)
        .mount(&server)
        .await;
    let issue: GitHubIssue = serde_json::from_value(issue(1, 101)).unwrap();
    let blockers = client(&server).dependencies("acme/app", &issue).await.unwrap();
    assert_eq!(blockers.len(), 101);
    assert_eq!(blockers.iter().filter(|b| !b.is_satisfied()).map(|b| b.key.as_str()).collect::<Vec<_>>(), vec!["acme/other#1"]);
}

#[tokio::test]
async fn malformed_summary_unknown_states_duplicates_and_partial_pages_fail_closed() {
    let server = MockServer::start().await;
    for rows in [
        json!([]),
        json!([null]),
        json!([dependency(&server, "acme/core", 9, "unknown")]),
        json!([dependency(&server, "acme/core", 9, "open"), dependency(&server, "acme/core", 9, "open")]),
        json!([{"number":9,"title":"bad","repository_url":"https://foreign.invalid/repos/acme/core","state":"closed"}]),
    ] {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues/1/dependencies/blocked_by"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rows))
            .mount(&server)
            .await;
        let issue = serde_json::from_value(issue(1, 1)).unwrap();
        assert!(client(&server).dependencies("acme/app", &issue).await.is_err());
    }
    let mut missing = issue(0, 0);
    missing.as_object_mut().unwrap().remove("issue_dependencies_summary");
    assert!(client(&server).dependencies("acme/app", &serde_json::from_value(missing).unwrap()).await.is_err());
    server.reset().await;
    Mock::given(method("GET"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).insert_header("Link", "<unused>; rel=\"next\"").set_body_json(vec![dependency(
            &server,
            "acme/core",
            9,
            "open",
        )]))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(query_param("page", "2")).respond_with(ResponseTemplate::new(403)).mount(&server).await;
    assert!(client(&server).dependencies("acme/app", &serde_json::from_value(issue(1, 1)).unwrap()).await.is_err());
}

#[tokio::test]
async fn source_wide_dependency_failure_does_not_partially_apply() {
    let server = MockServer::start().await;
    for status in [401, 403, 429, 500] {
        server.reset().await;
        snapshot(&server, issue(0, 0), ResponseTemplate::new(status)).await;
        let store = Store::open_in_memory().unwrap();
        assert!(plan_sync(&store, &config(&server), &client(&server)).await.is_err(), "{status}");
        assert_eq!((store.list_tasks().unwrap()).len(), 0);
    }
}

#[test]
fn stale_dependency_snapshots_cannot_claim_or_overwrite_a_launch() {
    let store = Store::open_in_memory().unwrap();
    let mut task = powerqueue::github::sync::task_from_issue("acme/app", &serde_json::from_value(issue(0, 0)).unwrap());
    task.state = TaskState::Crashed;
    task.github_admission = Some(GitHubAdmission { approved: true, reason: "approved".into() });
    store.insert_task(&task).unwrap();
    let stale = task.clone();
    let blockers = vec![powerqueue::domain::LinkedIssue {
        key: "acme/core#9".into(),
        title: "Prerequisite".into(),
        state_type: "started".into(),
        pr_merged: false,
    }];
    on_github_dependencies(&mut task, blockers, false);
    assert_eq!(task.state, TaskState::Crashed);
    assert!(store.record_github_admission(&stale, &task).unwrap());
    let mut starting = stale.clone();
    starting.state = TaskState::Starting;
    assert!(!store.claim_task_start(&stale, &starting).unwrap());
    assert!(!store.record_github_admission(&stale, &stale).unwrap());
    assert!(!store.update_source_task(&stale, &stale).unwrap());
    let mut setup = task.clone();
    setup.state = TaskState::Starting;
    setup.worktree_path = Some("/worktree".into());
    on_github_launch_held(&mut setup);
    assert_eq!(setup.state, TaskState::Blocked);
    assert_eq!(setup.worktree_path.as_deref(), Some("/worktree"));
    assert!(setup.github_admission.unwrap().approved);
    assert!(powerqueue::scheduler::pick_next(&[task], Utc::now()).is_none());
}

#[tokio::test]
async fn dependency_sync_preserves_a_live_rate_limited_session() {
    use powerqueue::domain::{ModelTier, Session, SessionState};
    let server = MockServer::start().await;
    let store = Store::open_in_memory().unwrap();
    let cfg = config(&server);
    snapshot(&server, issue(0, 0), ResponseTemplate::new(200).set_body_json(json!([]))).await;
    apply_plan(&store, &plan_sync(&store, &cfg, &client(&server)).await.unwrap()).unwrap();
    let mut task = store.get_task_by_key("acme/app#1").unwrap().unwrap();
    task.state = TaskState::Throttled;
    task.not_before = Some(Utc::now() + chrono::Duration::minutes(5));
    store.update_task(&task).unwrap();
    let now = Utc::now();
    store
        .insert_session(&Session {
            id: uuid::Uuid::new_v4(),
            task_id: task.id,
            attempt: 1,
            model: ModelTier::sonnet(),
            state: SessionState::Running,
            tmux_session: "pq".into(),
            tmux_window: "@none".into(),
            pane_id: None,
            pid: None,
            transcript_path: None,
            exit_code: None,
            started_at: now,
            ended_at: None,
            last_activity_at: now,
            error: None,
            agent_session_id: None,
            waiting_since: None,
            waited_secs: 0,
            runtime: powerqueue::domain::SessionRuntime::Tmux,
        })
        .unwrap();
    for state in ["open", "closed"] {
        server.reset().await;
        snapshot(
            &server,
            issue(usize::from(state == "open"), 1),
            ResponseTemplate::new(200).set_body_json(vec![dependency(&server, "acme/core", 9, state)]),
        )
        .await;
        let plan = plan_sync(&store, &cfg, &client(&server)).await.unwrap();
        assert_eq!(plan.changes[0].task.state, TaskState::Throttled);
        apply_plan(&store, &plan).unwrap();
        let saved = store.get_task(task.id).unwrap().unwrap();
        assert_eq!(saved.state, TaskState::Throttled);
        assert_eq!(saved.not_before.map(|t| t.timestamp_millis()), task.not_before.map(|t| t.timestamp_millis()));
        assert_eq!(saved.is_waiting(), state == "open");
    }
}
