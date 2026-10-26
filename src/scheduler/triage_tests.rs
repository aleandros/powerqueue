use super::*;
use crate::execution::{
    TriageOutcome,
    tests::{config, it_task},
};
use crate::secrets::FileBackend;
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

#[derive(Clone)]
struct Reports {
    comment: std::sync::Arc<std::sync::Mutex<Option<Value>>>,
    labels_available: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Respond for Reports {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = request.body_json().unwrap();
        let query = body["query"].as_str().unwrap();
        let data = if query.contains("comment(id:") {
            json!({"comment": self.comment.lock().unwrap().clone()})
        } else if query.contains("commentCreate") {
            let mut comment = self.comment.lock().unwrap();
            assert!(comment.is_none(), "report must not be duplicated after restart");
            let input = &body["variables"]["input"];
            assert!(input["body"].as_str().unwrap().starts_with("[interno]\n"));
            *comment = Some(json!({"id": input["id"], "body": input["body"], "issue": {"id": input["issueId"]}}));
            json!({"commentCreate": {"success": true}})
        } else if query.contains("issue(id:") {
            // The issue carries the intake label of the same single-select group, but no
            // other recommendation label; Linear rejects removing labels that are absent.
            json!({"issue": {"labels": {"nodes": [{"id": "requested", "name": "requested", "parent": {"id": "g-triage", "name": "triage"}}, {"id": "skip", "name": "skip", "parent": {"id": "g-agent", "name": "agent"}}], "pageInfo": {"hasNextPage": false}}}})
        } else if query.contains("issueLabels") {
            let nodes = if self.labels_available.load(Ordering::SeqCst) {
                json!([{"id": "needs-info", "name": "needs-information", "parent": {"id": "g-triage", "name": "triage"}}, {"id": "ready", "name": "ready-for-agent", "parent": {"id": "g-triage", "name": "triage"}}, {"id": "requested", "name": "requested", "parent": {"id": "g-triage", "name": "triage"}}, {"id": "skip", "name": "skip", "parent": {"id": "g-agent", "name": "agent"}}])
            } else {
                json!([])
            };
            json!({"issueLabels": {"nodes": nodes, "pageInfo": {"hasNextPage": false}}})
        } else if query.contains("issueUpdate") {
            assert_eq!(body["variables"]["input"], json!({"addedLabelIds": ["needs-info"], "removedLabelIds": ["requested"]}));
            json!({"issueUpdate": {"success": true}})
        } else {
            panic!("unexpected query (must never move or close issue): {query}");
        };
        ResponseTemplate::new(200).set_body_json(json!({"data": data}))
    }
}

#[tokio::test]
async fn triage_keeps_ticket_open_and_retries_delivery_after_restart_without_duplicate_comment() {
    let server = MockServer::start().await;
    let reports = Reports { comment: std::sync::Arc::default(), labels_available: std::sync::Arc::new(false.into()) };
    Mock::given(method("POST")).respond_with(reports.clone()).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(dir.path());
    let store = Store::open(&paths.database()).unwrap();
    let mut cfg = config();
    cfg.repo.path = dir.path().display().to_string();
    cfg.linear.endpoint = server.uri();
    cfg.linear.post_comments = crate::config::PostComments::Questions;
    let make_daemon = || {
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
        secrets.set(SecretKind::LinearApiKey, "test-key").unwrap();
        Daemon::new(cfg.clone(), paths.clone(), store.clone(), secrets).unwrap()
    };
    let mut task = it_task();
    transitions::on_execution_selected(&mut task, &cfg).unwrap();
    task.state = TaskState::Running;
    commands::on_triage_complete(&mut task, "Findings and draft customer questions", TriageOutcome::NeedsInformation, Utc::now())
        .unwrap();
    store.insert_task(&task).unwrap();
    let mut daemon = make_daemon();
    for target in [LinearTarget::InProgress, LinearTarget::Done, LinearTarget::Blocked] {
        daemon.update_issue(&task, target, Some("must not post routine completion".into())).await;
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    daemon.publish_triage(Utc::now()).await.unwrap();
    let key = crate::scheduler::triage::delivery_key(task.id);
    let mut receipt = store.kv_get::<crate::scheduler::triage::Delivery>(&key).unwrap().unwrap();
    assert!(receipt.comment_posted);
    assert!(!receipt.label_set);
    assert!(receipt.error.as_ref().unwrap().contains("expected one label"));
    // Simulate the hardest crash window: Linear saved the comment but its local acknowledgement was lost.
    receipt.comment_posted = false;
    receipt.next_attempt = Utc::now();
    store.kv_set(&key, &receipt).unwrap();
    drop(daemon);
    reports.labels_available.store(true, Ordering::SeqCst);
    let mut daemon = make_daemon();
    daemon.publish_triage(Utc::now()).await.unwrap();
    assert!(store.kv_get::<crate::scheduler::triage::Delivery>(&key).unwrap().unwrap().done());
    let count = server.received_requests().await.unwrap().len();
    daemon.publish_triage(Utc::now()).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), count);
    assert_eq!(store.get_task(task.id).unwrap().unwrap().state, TaskState::Completed);
}

#[tokio::test]
async fn triage_parent_is_not_closed_by_coding_child_completion() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(|request: &Request| {
        let body: Value = request.body_json().unwrap();
        assert!(!body["query"].as_str().unwrap().contains("mutation"));
        ResponseTemplate::new(200).set_body_json(json!({"data": {"issue": {
            "id": "parent", "identifier": "IT-1", "state": {"type": "started"}, "team": {"key": "IT"},
            "children": {"nodes": [{"identifier": "AVS-1", "title": "Fix", "state": {"type": "completed"}}], "pageInfo": {"hasNextPage": false}}
        }}}))
    }).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(dir.path());
    let store = Store::open_in_memory().unwrap();
    let cfg = config();
    let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
    let mut daemon = Daemon::new(cfg, paths, store.clone(), secrets).unwrap();
    let mut parents = WatchedParents::default();
    parents.watch(["IT-1".into()]);
    store.kv_set(WATCHED_PARENTS_KEY, &parents).unwrap();
    let client = LinearClient::new(server.uri(), "test-key").unwrap();
    daemon.close_finished_parents(&client, &[], true, Utc::now()).await.unwrap();
    assert!(!store.kv_get::<WatchedParents>(WATCHED_PARENTS_KEY).unwrap().unwrap().closed.contains(&"IT-1".into()));
}

#[tokio::test]
async fn failing_deliveries_do_not_starve_a_newly_completed_report() {
    let server = MockServer::start().await;
    let commented = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = std::sync::Arc::<std::sync::Mutex<std::vec::Vec<std::string::String>>>::clone(&commented);
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().unwrap();
            let query = body["query"].as_str().unwrap();
            let data = if query.contains("comment(id:") {
                json!({"comment": null})
            } else if query.contains("commentCreate") {
                seen.lock().unwrap().push(body["variables"]["input"]["issueId"].as_str().unwrap().to_owned());
                json!({"commentCreate": {"success": true}})
            } else {
                json!({"issueLabels": {"nodes": [], "pageInfo": {"hasNextPage": false}}})
            };
            ResponseTemplate::new(200).set_body_json(json!({"data": data}))
        })
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(dir.path());
    let store = Store::open(&paths.database()).unwrap();
    let mut cfg = config();
    cfg.repo.path = dir.path().display().to_string();
    cfg.linear.endpoint = server.uri();
    cfg.linear.post_comments = crate::config::PostComments::Questions;
    let now = Utc::now();
    let completed = |key: &str, issue: &str, at| {
        let mut task = Task::new(
            key,
            "Investigate",
            TaskSource::Linear {
                issue_id: issue.into(),
                identifier: key.into(),
                url: format!("https://linear.app/{key}"),
                team_key: "IT".into(),
            },
        );
        transitions::on_execution_selected(&mut task, &cfg).unwrap();
        task.state = TaskState::Running;
        commands::on_triage_complete(&mut task, "Findings", TriageOutcome::NeedsInformation, at).unwrap();
        store.insert_task(&task).unwrap();
        store.get_task(task.id).unwrap().unwrap()
    };
    // An older report whose label step keeps failing and whose retry is overdue.
    let stuck = completed("IT-1", "stuck-issue", now - Duration::days(2));
    let key = crate::scheduler::triage::delivery_key(stuck.id);
    let mut receipt = crate::scheduler::triage::Delivery::new(stuck.completed_at.unwrap());
    receipt.comment_posted = true;
    receipt.record_attempt(now - Duration::hours(3), Some("Label not on issue".into()));
    store.kv_set(&key, &receipt).unwrap();
    let fresh = completed("IT-2", "fresh-issue", now);
    let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
    secrets.set(SecretKind::LinearApiKey, "test-key").unwrap();
    let mut daemon = Daemon::new(cfg.clone(), paths.clone(), store.clone(), secrets).unwrap();
    daemon.publish_triage(now).await.unwrap();
    assert_eq!(*commented.lock().unwrap(), ["fresh-issue"]);
    assert_eq!(store.kv_get::<crate::scheduler::triage::Delivery>(&key).unwrap().unwrap().attempts, 1);
    let fresh_receipt =
        store.kv_get::<crate::scheduler::triage::Delivery>(&crate::scheduler::triage::delivery_key(fresh.id)).unwrap().unwrap();
    assert!(fresh_receipt.comment_posted && fresh_receipt.attempts == 1);
    // Next tick: the fresh report now waits for its backoff, so the overdue retry runs.
    daemon.publish_triage(now).await.unwrap();
    let stuck_receipt = store.kv_get::<crate::scheduler::triage::Delivery>(&key).unwrap().unwrap();
    assert_eq!(stuck_receipt.attempts, 2);
    assert_eq!(stuck_receipt.next_attempt, now + Duration::seconds(120));
}
