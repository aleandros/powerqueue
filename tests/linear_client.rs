//! `LinearClient` against a wiremock GraphQL server.

use powerqueue::linear::{CycleInfo, CycleScope, CycleStatus, IssueFilter, LinearClient};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const KEY: &str = "lin_api_test_key";

async fn server() -> MockServer {
    MockServer::start().await
}

fn client(server: &MockServer) -> LinearClient {
    LinearClient::new(format!("{}/graphql", server.uri()), KEY).expect("client")
}

fn issue(id: &str, ident: &str, labels: &[&str]) -> Value {
    json!({
        "id": id, "identifier": ident, "title": format!("Title {ident}"), "description": null,
        "url": format!("https://linear.app/t/issue/{ident}"), "priority": 2, "estimate": 3,
        "labels": { "nodes": labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>() },
        "state": { "name": "Todo", "type": "unstarted" }, "team": { "key": "ENG" },
        "project": { "name": "Launch" }, "assignee": { "id": "user-1" },
        "createdAt": "2026-09-01T10:00:00.000Z", "updatedAt": "2026-09-02T10:00:00.000Z"
    })
}

/// Responds based on the operation mentioned in the query string.
struct ByQuery(Vec<(&'static str, Value)>);

impl Respond for ByQuery {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = request.body_json().expect("json body");
        let query = body["query"].as_str().unwrap_or("");
        for (needle, data) in &self.0 {
            if query.contains(needle) {
                return ResponseTemplate::new(200).set_body_json(json!({ "data": data }));
            }
        }
        ResponseTemplate::new(200).set_body_json(json!({ "errors": [{ "message": format!("unexpected query: {query}") }] }))
    }
}

#[tokio::test]
async fn viewer_sends_key_without_bearer() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", KEY))
        .and(header("content-type", "application/json"))
        .and(body_partial_json(json!({ "variables": {} })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "viewer": { "id": "user-1", "name": "Edgar", "email": "edgar@example.com" } }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let v = client(&server).viewer().await.unwrap();
    assert_eq!(v.id, "user-1");
    assert_eq!(v.name, "Edgar");
    assert_eq!(v.email, "edgar@example.com");
}

#[tokio::test]
async fn teams_are_sorted_by_key() {
    let server = server().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "teams": { "nodes": [
                { "id": "t2", "key": "OPS", "name": "Operations" },
                { "id": "t1", "key": "ENG", "name": "Engineering" }
            ] } }
        })))
        .mount(&server)
        .await;
    let teams = client(&server).teams().await.unwrap();
    assert_eq!(teams.iter().map(|t| t.key.as_str()).collect::<Vec<_>>(), vec!["ENG", "OPS"]);
    assert_eq!(teams[0].name, "Engineering");
}

#[tokio::test]
async fn workflow_states_for_team() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "variables": { "key": "ENG" } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "workflowStates": { "nodes": [
                { "id": "s1", "name": "Todo", "type": "unstarted", "team": { "key": "ENG" } },
                { "id": "s2", "name": "Done", "type": "completed", "team": { "key": "ENG" } }
            ] } }
        })))
        .mount(&server)
        .await;
    let states = client(&server).workflow_states("ENG").await.unwrap();
    assert_eq!(states.len(), 2);
    assert_eq!(states[1].kind, "completed");
    assert_eq!(states[1].team_key, "ENG");
}

#[tokio::test]
async fn fetch_issues_paginates_and_filters_labels() {
    let server = server().await;
    // Page 1 (no cursor).
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "variables": { "after": null, "first": 50 } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "issues": {
                "nodes": [issue("u1", "ENG-1", &["agent"]), issue("u2", "ENG-2", &["Agent", "no-agent"])],
                "pageInfo": { "hasNextPage": true, "endCursor": "cursor-1" }
            } }
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Page 2.
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "variables": { "after": "cursor-1" } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "issues": {
                "nodes": [issue("u3", "ENG-3", &["AGENT"]), issue("u4", "ENG-4", &["other"])],
                "pageInfo": { "hasNextPage": false, "endCursor": null }
            } }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let filter = IssueFilter {
        team_keys: vec!["ENG".into()],
        state_names: vec!["Todo".into()],
        required_labels: vec!["agent".into()],
        excluded_labels: vec!["no-agent".into()],
        max: 100,
        ..Default::default()
    };
    let issues = client(&server).fetch_issues(&filter).await.unwrap();
    assert_eq!(issues.iter().map(|i| i.identifier.as_str()).collect::<Vec<_>>(), vec!["ENG-1", "ENG-3"]);
    assert_eq!(issues[0].description, "");
    assert_eq!(issues[0].project.as_deref(), Some("Launch"));
    assert_eq!(issues[0].estimate, Some(3.0));
    assert_eq!(issues[0].priority, 2);

    // The server filter carried the configured teams and states.
    let requests = server.received_requests().await.unwrap();
    let first: Value = requests[0].body_json().unwrap();
    assert_eq!(first["variables"]["filter"]["team"]["key"]["in"], json!(["ENG"]));
    assert_eq!(first["variables"]["filter"]["state"]["name"]["in"], json!(["Todo"]));
    assert!(first["variables"]["filter"].get("assignee").is_none());
}

#[tokio::test]
async fn fetch_issues_resolves_me_and_stops_at_max() {
    let server = server().await;
    Mock::given(method("POST"))
        .respond_with(ByQuery(vec![
            ("viewer", json!({ "viewer": { "id": "user-9", "name": "Me", "email": "me@example.com" } })),
            (
                "issues(",
                json!({ "issues": {
                    "nodes": [issue("u1", "ENG-1", &[]), issue("u2", "ENG-2", &[]), issue("u3", "ENG-3", &[])],
                    "pageInfo": { "hasNextPage": true, "endCursor": "c" }
                } }),
            ),
        ]))
        .mount(&server)
        .await;
    let filter = IssueFilter { assignee: Some("me".into()), max: 2, ..Default::default() };
    let issues = client(&server).fetch_issues(&filter).await.unwrap();
    assert_eq!(issues.len(), 2);
    let requests = server.received_requests().await.unwrap();
    let issue_req = requests
        .iter()
        .map(|r| r.body_json::<Value>().unwrap())
        .find(|b| b["query"].as_str().unwrap().contains("issues("))
        .unwrap();
    assert_eq!(issue_req["variables"]["filter"]["assignee"]["id"]["eq"], "user-9");
    assert_eq!(issue_req["variables"]["first"], 2);
    // Only one issues page was requested because max was reached.
    assert_eq!(
        requests.iter().filter(|r| r.body_json::<Value>().unwrap()["query"].as_str().unwrap().contains("issues(")).count(),
        1
    );
}

#[tokio::test]
async fn retries_after_429_with_retry_after() {
    let server = server().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0").set_body_string("slow down"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "viewer": { "id": "u", "name": "N", "email": "e@x" } }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let v = client(&server).viewer().await.unwrap();
    assert_eq!(v.id, "u");
}

#[tokio::test]
async fn gives_up_after_repeated_5xx() {
    let server = server().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(503).set_body_string("down")).expect(4).mount(&server).await;
    let err = client(&server).viewer().await.unwrap_err().to_string();
    assert!(err.contains("503"), "{err}");
    assert!(err.contains("attempts"), "{err}");
}

#[tokio::test]
async fn graphql_errors_are_surfaced() {
    let server = server().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{ "message": "Argument Validation Error" }, { "message": "unknown field `bogus`" }]
        })))
        .mount(&server)
        .await;
    let err = client(&server).viewer().await.unwrap_err().to_string();
    assert!(err.contains("Argument Validation Error"), "{err}");
    assert!(err.contains("unknown field `bogus`"), "{err}");
}

#[tokio::test]
async fn unauthorized_is_a_clear_error_without_retry() {
    let server = server().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({ "errors": [{ "message": "Authentication required" }] })))
        .expect(1)
        .mount(&server)
        .await;
    let err = client(&server).viewer().await.unwrap_err().to_string();
    assert!(err.contains("401"), "{err}");
    assert!(err.contains("API key"), "{err}");
}

#[tokio::test]
async fn set_state_resolves_name_case_insensitively() {
    let server = server().await;
    Mock::given(method("POST"))
        .respond_with(ByQuery(vec![
            ("issue(id", json!({ "issue": issue("u1", "ENG-1", &[]) })),
            (
                "workflowStates",
                json!({ "workflowStates": { "nodes": [
                    { "id": "s1", "name": "Todo", "type": "unstarted", "team": { "key": "ENG" } },
                    { "id": "s2", "name": "In Progress", "type": "started", "team": { "key": "ENG" } }
                ] } }),
            ),
            ("issueUpdate", json!({ "issueUpdate": { "success": true } })),
        ]))
        .mount(&server)
        .await;
    let c = client(&server);
    c.set_state("u1", "in progress").await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let update = requests
        .iter()
        .map(|r| r.body_json::<Value>().unwrap())
        .find(|b| b["query"].as_str().unwrap().contains("issueUpdate"))
        .unwrap();
    assert_eq!(update["variables"]["stateId"], "s2");

    let err = c.set_state("u1", "Done").await.unwrap_err().to_string();
    assert!(err.contains("Todo") && err.contains("In Progress"), "{err}");
}

#[tokio::test]
async fn comment_and_missing_issue() {
    let server = server().await;
    Mock::given(method("POST"))
        .respond_with(ByQuery(vec![
            ("commentCreate", json!({ "commentCreate": { "success": true } })),
            ("issue(id", json!({ "issue": null })),
        ]))
        .mount(&server)
        .await;
    let c = client(&server);
    c.comment("u1", "hello **world**").await.unwrap();
    assert!(c.get_issue("missing").await.unwrap().is_none());
    let requests = server.received_requests().await.unwrap();
    let body: Value = requests[0].body_json().unwrap();
    assert_eq!(body["variables"]["body"], "hello **world**");
}

#[tokio::test]
async fn fetch_issues_sends_cycle_and_project_filters_and_parses_cycles() {
    let server = server().await;
    let mut in_cycle = issue("u1", "ENG-1", &[]);
    in_cycle["cycle"] =
        json!({ "number": 14, "name": "Sprint 14", "isActive": true, "isNext": false, "isPast": false, "isFuture": false });
    let mut no_cycle = issue("u2", "ENG-2", &[]);
    no_cycle["cycle"] = Value::Null;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "issues": {
                "nodes": [in_cycle, no_cycle],
                "pageInfo": { "hasNextPage": false, "endCursor": null }
            } }
        })))
        .mount(&server)
        .await;

    let filter = IssueFilter {
        team_keys: vec!["ENG".into()],
        state_names: vec!["Todo".into()],
        cycle: CycleScope::ActiveOrNext,
        projects: vec!["Launch".into()],
        ..Default::default()
    };
    let issues = client(&server).fetch_issues(&filter).await.unwrap();
    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0].cycle, Some(CycleInfo { number: 14, name: Some("Sprint 14".into()), status: CycleStatus::Active }));
    assert_eq!(issues[0].cycle_status(), Some("active"));
    assert_eq!(issues[1].cycle, None);

    let requests = server.received_requests().await.unwrap();
    let body: Value = requests[0].body_json().unwrap();
    let query = body["query"].as_str().unwrap();
    assert!(query.contains("cycle { number name isActive isNext isPast isFuture }"), "{query}");
    assert_eq!(
        body["variables"]["filter"],
        json!({
            "team": { "key": { "in": ["ENG"] } },
            "state": { "name": { "in": ["Todo"] } },
            "project": { "name": { "in": ["Launch"] } },
            "or": [ { "cycle": { "isActive": { "eq": true } } }, { "cycle": { "isNext": { "eq": true } } } ]
        })
    );

    // Single-cycle scopes send one `cycle` filter.
    for (scope, expected) in [
        (CycleScope::Active, json!({ "isActive": { "eq": true } })),
        (CycleScope::Next, json!({ "isNext": { "eq": true } })),
        (CycleScope::None, json!({ "null": true })),
    ] {
        let filter = IssueFilter { cycle: scope, ..Default::default() };
        client(&server).fetch_issues(&filter).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: Value = requests.last().unwrap().body_json().unwrap();
        assert_eq!(body["variables"]["filter"], json!({ "cycle": expected }), "{scope:?}");
    }

    // The default (`any`) sends no cycle or project filter at all.
    client(&server).fetch_issues(&IssueFilter::default()).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = requests.last().unwrap().body_json().unwrap();
    assert_eq!(body["variables"]["filter"], json!({}));
}
