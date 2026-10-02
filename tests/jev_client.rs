//! `JevClient` against a wiremock server.

use powerqueue::domain::{Task, TaskSource};
use powerqueue::jev::{JevClient, JevQuestion};
use powerqueue::priority::JevSection;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The sample response from docs/reference/linear-jev.md.
fn sample_response() -> Value {
    json!({
        "model": "jev-1.13.0",
        "answers": { "priority": { "type": "score", "score": 1.43,
            "legend": { "0": "can wait", "1": "nice to have", "2": "important", "3": "blocking" },
            "probabilities": { "0": 0.0, "1": 0.57, "2": 0.43, "3": 0.0 }, "confidence": 0.35 } },
        "usage": { "input_tokens": 210, "output_tokens": 31 }
    })
}

fn client(server: &MockServer) -> JevClient {
    JevClient::new(format!("{}/v1/systemone", server.uri()), "jev_test_key", "jev-latest").expect("client")
}

fn question() -> JevQuestion {
    JevQuestion {
        instructions: "How important is it to ship `title` this week?".into(),
        levels: vec!["can wait".into(), "nice to have".into(), "important".into(), "blocking".into()],
    }
}

#[tokio::test]
async fn score_parses_sample_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer jev_test_key"))
        .and(body_partial_json(json!({
            "model": "jev-latest",
            "state": { "title": "Fix login", "labels": ["bug"], "priority": "high" },
            "questions": { "priority": { "type": "score", "criteria": ["can wait", "nice to have", "important", "blocking"] } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(sample_response()))
        .expect(1)
        .mount(&server)
        .await;
    let state = json!({ "title": "Fix login", "description": "...", "labels": ["bug"], "priority": "high" });
    let score = client(&server).score(&state, &question()).await.unwrap();
    assert_eq!(score.score, 1.43);
    assert_eq!(score.level_index, 1);
    assert_eq!(score.level, "nice to have");
    assert_eq!(score.confidence, 0.35);
    assert_eq!(score.probabilities, vec![0.0, 0.57, 0.43, 0.0]);
    assert_eq!(score.input_tokens, 210);
    assert!((score.normalized() - 1.43 / 3.0).abs() < 1e-9);
    assert_eq!(score.raw["model"], "jev-1.13.0");
}

#[tokio::test]
async fn unauthorized_is_reported_as_invalid_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({ "error": "invalid api key" })))
        .mount(&server)
        .await;
    let c = client(&server);
    let err = c.score(&json!({ "title": "x" }), &question()).await.unwrap_err().to_string();
    assert!(err.contains("401"), "{err}");
    assert!(err.to_lowercase().contains("invalid"), "{err}");
    let err = c.ping().await.unwrap_err().to_string();
    assert!(err.contains("401"), "{err}");
}

#[tokio::test]
async fn ping_sends_two_level_question() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "state": { "state": "ping" } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "answers": { "priority": { "type": "score", "score": 0.9, "probabilities": [0.1, 0.9] } }
        })))
        .expect(1)
        .mount(&server)
        .await;
    client(&server).ping().await.unwrap();
    let body: Value = server.received_requests().await.unwrap()[0].body_json().unwrap();
    assert_eq!(body["questions"]["priority"]["criteria"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn other_http_errors_include_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500).set_body_string("boom")).mount(&server).await;
    let err = client(&server).score(&json!({}), &question()).await.unwrap_err().to_string();
    assert!(err.contains("500") && err.contains("boom"), "{err}");
}

#[tokio::test]
async fn score_task_builds_state_from_task() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({
            "state": { "title": "Fix login", "labels": ["bug", "customer"], "priority": "urgent", "estimate": 5.0, "project": "Launch" },
            "questions": { "priority": { "instructions": "How urgent?", "criteria": ["low", "high"] } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "answers": { "priority": { "score": 0.8, "legend": ["low", "high"], "probabilities": { "0": 0.2, "1": 0.8 }, "confidence": 0.8 } },
            "usage": { "input_tokens": 50 }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut task = Task::new("ENG-1", "Fix login", TaskSource::Manual);
    task.description = "d".repeat(5000);
    task.labels = vec!["bug".into(), "customer".into()];
    task.linear_priority = Some(1);
    task.estimate = Some(5.0);
    task.project = Some("Launch".into());
    let section = JevSection { enabled: true, question: "How urgent?".into(), levels: vec!["low".into(), "high".into()] };
    let score = client(&server).score_task(&task, &section).await.unwrap();
    assert_eq!(score.level, "high");
    assert_eq!(score.level_index, 1);
    assert_eq!(score.input_tokens, 50);
    let body: Value = server.received_requests().await.unwrap()[0].body_json().unwrap();
    assert_eq!(body["state"]["description"].as_str().unwrap().len(), 4000);
}
