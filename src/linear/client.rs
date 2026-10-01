//! Thin async GraphQL client for Linear.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The authenticated user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewer {
    pub id: String,
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub key: String,
    pub name: String,
}

/// A workflow state of a team (`type` is `backlog|unstarted|started|completed|canceled|triage`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowState {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub team_key: String,
}

/// The subset of a Linear issue powerqueue cares about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinearIssue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: String,
    pub url: String,
    /// 0 = none, 1 = urgent, 2 = high, 3 = normal, 4 = low.
    pub priority: u8,
    pub estimate: Option<f64>,
    pub labels: Vec<String>,
    pub state_name: String,
    pub state_type: String,
    pub team_key: String,
    pub project: Option<String>,
    pub assignee_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Server-side filter for [`LinearClient::fetch_issues`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IssueFilter {
    pub team_keys: Vec<String>,
    /// `Some("me")` resolves to the viewer; otherwise a user id.
    pub assignee: Option<String>,
    /// Workflow state names (case-insensitive on our side).
    pub state_names: Vec<String>,
    pub required_labels: Vec<String>,
    pub excluded_labels: Vec<String>,
    pub max: u32,
}

/// Async client. Cheap to clone (shares the reqwest pool).
#[derive(Debug, Clone)]
pub struct LinearClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl LinearClient {
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(format!("powerqueue/{}", crate::VERSION))
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self { http, endpoint: endpoint.into(), api_key: api_key.into() })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Execute a raw GraphQL request and return `data`. GraphQL `errors`
    /// and HTTP failures become `Err`; 429 is retried with backoff.
    pub async fn graphql(&self, query: &str, variables: serde_json::Value) -> Result<serde_json::Value> {
        let _ = (&self.http, query, variables);
        todo!("TODO(agent-linear): implement graphql() with retries on 429/5xx")
    }

    pub async fn viewer(&self) -> Result<Viewer> {
        todo!("TODO(agent-linear)")
    }

    pub async fn teams(&self) -> Result<Vec<Team>> {
        todo!("TODO(agent-linear)")
    }

    pub async fn workflow_states(&self, team_key: &str) -> Result<Vec<WorkflowState>> {
        let _ = team_key;
        todo!("TODO(agent-linear)")
    }

    /// Fetch issues matching `filter`, paginating until `filter.max`.
    pub async fn fetch_issues(&self, filter: &IssueFilter) -> Result<Vec<LinearIssue>> {
        let _ = filter;
        todo!("TODO(agent-linear)")
    }

    pub async fn get_issue(&self, id: &str) -> Result<Option<LinearIssue>> {
        let _ = id;
        todo!("TODO(agent-linear)")
    }

    /// Move an issue to the workflow state called `state_name` in its team.
    pub async fn set_state(&self, issue_id: &str, state_name: &str) -> Result<()> {
        let _ = (issue_id, state_name);
        todo!("TODO(agent-linear)")
    }

    pub async fn comment(&self, issue_id: &str, body: &str) -> Result<()> {
        let _ = (issue_id, body);
        todo!("TODO(agent-linear)")
    }
}
