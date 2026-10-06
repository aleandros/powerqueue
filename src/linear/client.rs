//! Thin async GraphQL client for Linear.
//!
//! Every call goes through [`LinearClient::graphql`], which handles
//! authentication, retries (429 with `Retry-After`, 5xx, transport errors)
//! and turns GraphQL `errors` into `Err`. The API key is never logged.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

pub use crate::config::CycleScope;
use crate::domain::LinkedIssue;

/// The authenticated user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewer {
    pub id: String,
    pub name: String,
    pub email: String,
}

/// A Linear team.
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
    /// Label names; child labels are qualified as `parent/name`
    /// (see [`crate::domain::qualified_label`]).
    pub labels: Vec<String>,
    pub state_name: String,
    pub state_type: String,
    pub team_key: String,
    pub project: Option<String>,
    pub assignee_id: Option<String>,
    /// The issue's cycle, if it is in one.
    #[serde(default)]
    pub cycle: Option<CycleInfo>,
    /// Issues that block this one (Linear `blocks` relations pointing here),
    /// with their state. `pr_merged` is filled in by the daemon
    /// ([`LinearClient::pr_merged`]) for blockers that are still open.
    #[serde(default)]
    pub blocked_by: Vec<LinkedIssue>,
    /// Sub-issues; non-empty makes the task a container.
    #[serde(default)]
    pub children: Vec<LinkedIssue>,
    /// Identifier of the parent issue, if this is a sub-issue.
    #[serde(default)]
    pub parent: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl LinearIssue {
    /// True if the issue carries `label` (case-insensitive; see
    /// [`crate::domain::label_matches`] for how `parent/name` is handled).
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| crate::domain::label_matches(l, label))
    }

    /// The cycle status word stored on tasks (`active`, `next`, `past`,
    /// `future`, `other`); `None` when the issue is in no cycle.
    pub fn cycle_status(&self) -> Option<&'static str> {
        self.cycle.as_ref().map(|c| c.status.as_str())
    }
}

/// Where a cycle sits relative to today, from Linear's `isActive` /
/// `isNext` / `isPast` / `isFuture` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CycleStatus {
    /// The team's current cycle.
    Active,
    /// The cycle right after the active one.
    Next,
    /// Already ended.
    Past,
    /// Starts later than the next cycle.
    Future,
    /// None of the flags were set (should not happen; kept for safety).
    Other,
}

impl CycleStatus {
    /// Derive from the booleans Linear returns; `isActive` wins, then
    /// `isNext`, `isPast`, `isFuture`.
    pub fn from_flags(is_active: bool, is_next: bool, is_past: bool, is_future: bool) -> Self {
        if is_active {
            Self::Active
        } else if is_next {
            Self::Next
        } else if is_past {
            Self::Past
        } else if is_future {
            Self::Future
        } else {
            Self::Other
        }
    }

    /// Lower-case word as used in `PRIORITY.md` (`cycle: active`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Next => "next",
            Self::Past => "past",
            Self::Future => "future",
            Self::Other => "other",
        }
    }
}

impl std::fmt::Display for CycleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The cycle an issue belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleInfo {
    /// Linear's `Cycle.number` (a float in the schema; always integral).
    pub number: u32,
    pub name: Option<String>,
    pub status: CycleStatus,
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
    /// Which cycles to pull from (server-side).
    pub cycle: CycleScope,
    /// Project names to restrict to (server-side, `project.name in [...]`); empty = any.
    pub projects: Vec<String>,
    /// Maximum number of issues returned; `0` means no limit.
    pub max: u32,
}

impl IssueFilter {
    /// Build the filter from the `[linear]` section of the configuration.
    pub fn from_config(cfg: &crate::config::LinearConfig) -> Self {
        Self {
            team_keys: cfg.team_keys.clone(),
            assignee: cfg.assignee.clone(),
            state_names: cfg.queued_states.clone(),
            required_labels: cfg.required_labels.clone(),
            excluded_labels: cfg.excluded_labels.clone(),
            cycle: cfg.cycle_scope(),
            projects: cfg.project_names(),
            max: cfg.max_issues,
        }
    }

    /// The `IssueFilter` GraphQL input this filter sends, minus the
    /// assignee (which needs a network round-trip for `me`). Exposed so the
    /// shape can be unit-tested and shown by `linear sync`.
    pub fn server_filter(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut f = serde_json::Map::new();
        if !self.team_keys.is_empty() {
            f.insert("team".into(), serde_json::json!({ "key": { "in": self.team_keys } }));
        }
        if !self.state_names.is_empty() {
            f.insert("state".into(), serde_json::json!({ "name": { "in": self.state_names } }));
        }
        if !self.projects.is_empty() {
            f.insert("project".into(), serde_json::json!({ "name": { "in": self.projects } }));
        }
        match self.cycle {
            CycleScope::Any => {}
            CycleScope::Active => {
                f.insert("cycle".into(), serde_json::json!({ "isActive": { "eq": true } }));
            }
            CycleScope::Next => {
                f.insert("cycle".into(), serde_json::json!({ "isNext": { "eq": true } }));
            }
            CycleScope::ActiveOrNext => {
                f.insert(
                    "or".into(),
                    serde_json::json!([
                        { "cycle": { "isActive": { "eq": true } } },
                        { "cycle": { "isNext": { "eq": true } } }
                    ]),
                );
            }
            CycleScope::None => {
                f.insert("cycle".into(), serde_json::json!({ "null": true }));
            }
        }
        f
    }

    /// Client-side label check: at least one required label (if any are
    /// configured) and none of the excluded ones. Case-insensitive.
    pub fn labels_allow(&self, issue: &LinearIssue) -> bool {
        if self.excluded_labels.iter().any(|l| issue.has_label(l)) {
            return false;
        }
        self.required_labels.is_empty() || self.required_labels.iter().any(|l| issue.has_label(l))
    }
}

/// Fields requested for every issue query; shared by list and single lookups.
/// Sub-issues and relations are *not* here: nested connections on a page of
/// 50 issues would blow Linear's query complexity limit, so they come from
/// [`LinearClient::fetch_dependencies`] in small batches.
const ISSUE_FIELDS: &str = "id identifier title description url priority estimate \
     labels { nodes { name parent { name } } } state { name type } team { key } \
     project { name } assignee { id } \
     cycle { number name isActive isNext isPast isFuture } \
     parent { identifier } createdAt updatedAt";

/// Linear relation type meaning "`issue` blocks `relatedIssue`".
const BLOCKS_RELATION: &str = "blocks";

/// Nodes of a `children` connection.
const CHILD_NODE: &str = "identifier title state { type }";
/// Nodes of an `inverseRelations` connection: relations *pointing at* the
/// issue; a `blocks` one means "blocked by `issue`".
const RELATION_NODE: &str = "type issue { identifier title state { type } }";
/// Page size of the nested `children` / `inverseRelations` connections.
const DEPENDENCY_PAGE: u32 = 50;
/// Issues per dependency query. Each issue carries two connections of up to
/// [`DEPENDENCY_PAGE`] nodes, so this keeps a query far below Linear's
/// complexity limit (10 000 points).
const DEPENDENCY_BATCH: usize = 10;

/// Every sub-issue and every `blocked by` issue of one issue (all pages).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueDependencies {
    pub blocked_by: Vec<LinkedIssue>,
    pub children: Vec<LinkedIssue>,
}

/// A parent issue as seen by [`LinearClient::parent_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentStatus {
    pub id: String,
    pub identifier: String,
    /// Workflow state type of the parent itself.
    pub state_type: String,
    pub team_key: String,
    /// Every sub-issue (all pages).
    pub children: Vec<LinkedIssue>,
}

/// Page size for issue pagination.
const PAGE_SIZE: u32 = 50;
/// Total attempts per GraphQL request (1 initial + 3 retries).
const MAX_ATTEMPTS: u32 = 4;
/// Base backoff; doubles per retry.
const BASE_BACKOFF: Duration = Duration::from_millis(500);
/// Never sleep longer than this on a single retry, whatever `Retry-After` says.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Raw shape of an issue as returned by the API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIssue {
    id: String,
    identifier: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    url: String,
    #[serde(default)]
    priority: Option<f64>,
    #[serde(default)]
    estimate: Option<f64>,
    #[serde(default)]
    labels: Option<Nodes<RawLabel>>,
    #[serde(default)]
    state: Option<RawState>,
    #[serde(default)]
    team: Option<Keyed>,
    #[serde(default)]
    project: Option<Named>,
    #[serde(default)]
    assignee: Option<Ided>,
    #[serde(default)]
    cycle: Option<RawCycle>,
    #[serde(default)]
    parent: Option<RawIdentifier>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct RawIdentifier {
    identifier: String,
}

/// A related issue as nested in `children` / relations.
#[derive(Debug, Deserialize)]
struct RawLinked {
    identifier: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    state: Option<RawStateType>,
}

#[derive(Debug, Deserialize)]
struct RawStateType {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct RawRelation {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    issue: Option<RawLinked>,
}

/// A connection page: nodes plus whether more pages follow.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawConnection<T> {
    nodes: Vec<T>,
    #[serde(default)]
    page_info: Option<PageInfo>,
}

impl<T> RawConnection<T> {
    /// Cursor of the next page, if there is one.
    fn next_cursor(&self) -> Option<String> {
        self.page_info.as_ref().filter(|p| p.has_next_page).and_then(|p| p.end_cursor.clone())
    }
}

/// One node of a dependency query (see [`LinearClient::fetch_dependencies`]).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDependencies {
    id: String,
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    state: Option<RawStateType>,
    #[serde(default)]
    team: Option<Keyed>,
    #[serde(default)]
    children: Option<RawConnection<RawLinked>>,
    #[serde(default)]
    inverse_relations: Option<RawConnection<RawRelation>>,
}

/// The `blocks` relations of a page, as blockers.
fn blockers_of(relations: Vec<RawRelation>) -> impl Iterator<Item = LinkedIssue> {
    relations
        .into_iter()
        .filter(|rel| rel.kind.eq_ignore_ascii_case(BLOCKS_RELATION))
        .filter_map(|rel| rel.issue.map(LinkedIssue::from))
}

impl From<RawLinked> for LinkedIssue {
    fn from(raw: RawLinked) -> Self {
        LinkedIssue {
            key: raw.identifier,
            title: raw.title.unwrap_or_default(),
            state_type: raw.state.map(|s| s.kind).unwrap_or_default(),
            pr_merged: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCycle {
    #[serde(default)]
    number: Option<f64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    is_next: bool,
    #[serde(default)]
    is_past: bool,
    #[serde(default)]
    is_future: bool,
}

impl From<RawCycle> for CycleInfo {
    fn from(raw: RawCycle) -> Self {
        CycleInfo {
            number: raw.number.filter(|n| n.is_finite() && *n >= 0.0).map(|n| n.round() as u32).unwrap_or(0),
            name: raw.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
            status: CycleStatus::from_flags(raw.is_active, raw.is_next, raw.is_past, raw.is_future),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct Named {
    name: String,
}

/// A label with its group, if it is a child label.
#[derive(Debug, Deserialize)]
struct RawLabel {
    name: String,
    #[serde(default)]
    parent: Option<Named>,
}

#[derive(Debug, Deserialize)]
struct Keyed {
    key: String,
}

#[derive(Debug, Deserialize)]
struct Ided {
    id: String,
}

#[derive(Debug, Deserialize)]
struct RawState {
    name: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssuePage {
    nodes: Vec<RawIssue>,
    page_info: PageInfo,
}

impl From<RawIssue> for LinearIssue {
    fn from(raw: RawIssue) -> Self {
        let (state_name, state_type) = raw.state.map(|s| (s.name, s.kind)).unwrap_or_default();
        LinearIssue {
            id: raw.id,
            identifier: raw.identifier,
            title: raw.title,
            description: raw.description.unwrap_or_default(),
            url: raw.url,
            priority: raw.priority.map(|p| p.clamp(0.0, 4.0) as u8).unwrap_or(0),
            estimate: raw.estimate,
            labels: raw
                .labels
                .map(|l| {
                    l.nodes
                        .into_iter()
                        .map(|n| crate::domain::qualified_label(&n.name, n.parent.as_ref().map(|p| p.name.as_str())))
                        .collect()
                })
                .unwrap_or_default(),
            state_name,
            state_type,
            team_key: raw.team.map(|t| t.key).unwrap_or_default(),
            project: raw.project.map(|p| p.name),
            assignee_id: raw.assignee.map(|a| a.id),
            cycle: raw.cycle.map(CycleInfo::from),
            blocked_by: Vec::new(),
            children: Vec::new(),
            parent: raw.parent.map(|p| p.identifier),
            created_at: raw.created_at,
            updated_at: raw.updated_at,
        }
    }
}

/// Async client. Cheap to clone (shares the reqwest pool).
#[derive(Debug, Clone)]
pub struct LinearClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl LinearClient {
    /// Create a client for `endpoint` authenticating with `api_key`.
    /// Fails only if the HTTP client cannot be built (TLS backend problems).
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(format!("powerqueue/{}", crate::VERSION))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, endpoint: endpoint.into(), api_key: api_key.into() })
    }

    /// The GraphQL endpoint this client talks to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Execute a raw GraphQL request and return `data`. GraphQL `errors`
    /// and HTTP failures become `Err`; 429 (honouring `Retry-After`), 5xx
    /// and transport errors are retried up to three times with exponential
    /// backoff.
    pub async fn graphql(&self, query: &str, variables: serde_json::Value) -> Result<serde_json::Value> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let op = operation_name(query);
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            debug!(target: "powerqueue::linear", endpoint = %self.endpoint, operation = %op, attempt, "graphql request");
            let sent = self
                .http
                .post(&self.endpoint)
                .header(reqwest::header::AUTHORIZATION, self.api_key.as_str())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await;

            let response = match sent {
                Ok(r) => r,
                Err(e) if attempt < MAX_ATTEMPTS && (e.is_connect() || e.is_timeout()) => {
                    let wait = backoff_for(attempt, None);
                    warn!(target: "powerqueue::linear", operation = %op, error = %e, wait_ms = wait.as_millis() as u64, "transport error; retrying");
                    tokio::time::sleep(wait).await;
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("POST {} ({op})", self.endpoint)),
            };

            let status = response.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                if attempt < MAX_ATTEMPTS {
                    let wait = backoff_for(attempt, retry_after);
                    warn!(target: "powerqueue::linear", operation = %op, status = status.as_u16(), wait_ms = wait.as_millis() as u64, "retrying");
                    tokio::time::sleep(wait).await;
                    continue;
                }
                let text = response.text().await.unwrap_or_default();
                bail!("Linear returned HTTP {status} for {op} after {attempt} attempts: {}", excerpt(&text));
            }

            let text = response.text().await.with_context(|| format!("read response body for {op}"))?;
            let parsed: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) if !status.is_success() => {
                    bail!("Linear returned HTTP {status} for {op}: {}", excerpt(&text))
                }
                Err(e) => return Err(anyhow!("Linear returned invalid JSON for {op}: {e}: {}", excerpt(&text))),
            };

            if let Some(errors) = parsed.get("errors").and_then(|e| e.as_array())
                && !errors.is_empty()
            {
                let messages: Vec<String> = errors
                    .iter()
                    .map(|e| e.get("message").and_then(|m| m.as_str()).map(str::to_string).unwrap_or_else(|| e.to_string()))
                    .collect();
                if status == reqwest::StatusCode::UNAUTHORIZED {
                    bail!("Linear rejected the API key (HTTP 401): {}", messages.join("; "));
                }
                bail!("Linear GraphQL error for {op}: {}", messages.join("; "));
            }
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Linear rejected the API key (HTTP 401)");
            }
            if !status.is_success() {
                bail!("Linear returned HTTP {status} for {op}: {}", excerpt(&text));
            }
            return match parsed.get("data") {
                Some(data) if !data.is_null() => Ok(data.clone()),
                _ => Err(anyhow!("Linear response for {op} has no `data`: {}", excerpt(&text))),
            };
        }
    }

    /// The user the API key belongs to.
    pub async fn viewer(&self) -> Result<Viewer> {
        let data = self.graphql("query { viewer { id name email } }", serde_json::json!({})).await?;
        let viewer = data.get("viewer").cloned().ok_or_else(|| anyhow!("viewer missing from response"))?;
        serde_json::from_value(viewer).context("parse viewer")
    }

    /// All teams visible to the API key, sorted by key.
    pub async fn teams(&self) -> Result<Vec<Team>> {
        let data = self.graphql("query { teams { nodes { id key name } } }", serde_json::json!({})).await?;
        let nodes = data.pointer("/teams/nodes").cloned().ok_or_else(|| anyhow!("teams missing from response"))?;
        let mut teams: Vec<Team> = serde_json::from_value(nodes).context("parse teams")?;
        teams.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(teams)
    }

    /// Workflow states of the team with key `team_key` (exact key match on the server).
    pub async fn workflow_states(&self, team_key: &str) -> Result<Vec<WorkflowState>> {
        #[derive(Deserialize)]
        struct RawWs {
            id: String,
            name: String,
            #[serde(rename = "type")]
            kind: String,
            team: Option<Keyed>,
        }
        let query = "query($key: String!) { workflowStates(filter: { team: { key: { eq: $key } } }) { nodes { id name type team { key } } } }";
        let data = self.graphql(query, serde_json::json!({ "key": team_key })).await?;
        let nodes =
            data.pointer("/workflowStates/nodes").cloned().ok_or_else(|| anyhow!("workflowStates missing from response"))?;
        let raw: Vec<RawWs> = serde_json::from_value(nodes).context("parse workflow states")?;
        Ok(raw
            .into_iter()
            .map(|w| WorkflowState {
                id: w.id,
                name: w.name,
                kind: w.kind,
                team_key: w.team.map(|t| t.key).unwrap_or_else(|| team_key.to_string()),
            })
            .collect())
    }

    /// Fetch issues matching `filter`, paginating in pages of 50 until
    /// `filter.max` issues pass the client-side label filter (or the server
    /// runs out of pages). Teams, states, projects and the cycle scope are
    /// filtered server-side ([`IssueFilter::server_filter`]);
    /// `assignee = "me"` is resolved to the viewer id.
    pub async fn fetch_issues(&self, filter: &IssueFilter) -> Result<Vec<LinearIssue>> {
        let mut server_filter = filter.server_filter();
        if let Some(assignee) = filter.assignee.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
            let id = if assignee.eq_ignore_ascii_case("me") { self.viewer().await?.id } else { assignee.to_string() };
            server_filter.insert("assignee".into(), serde_json::json!({ "id": { "eq": id } }));
        }

        let query = format!(
            "query($filter: IssueFilter, $first: Int, $after: String) {{ \
               issues(filter: $filter, first: $first, after: $after, orderBy: updatedAt) {{ \
                 nodes {{ {ISSUE_FIELDS} }} pageInfo {{ hasNextPage endCursor }} }} }}"
        );
        let limit = if filter.max == 0 { usize::MAX } else { filter.max as usize };
        let mut out: Vec<LinearIssue> = Vec::new();
        let mut after: Option<String> = None;
        let mut pages = 0u32;
        loop {
            let remaining = limit.saturating_sub(out.len());
            let first = (remaining.min(PAGE_SIZE as usize) as u32).max(1);
            let variables = serde_json::json!({ "filter": server_filter, "first": first, "after": after });
            let data = self.graphql(&query, variables).await?;
            let page = data.get("issues").cloned().ok_or_else(|| anyhow!("issues missing from response"))?;
            let page: IssuePage = serde_json::from_value(page).context("parse issues page")?;
            pages += 1;
            let fetched = page.nodes.len();
            for raw in page.nodes {
                let issue = LinearIssue::from(raw);
                if filter.labels_allow(&issue) {
                    out.push(issue);
                }
                if out.len() >= limit {
                    break;
                }
            }
            debug!(target: "powerqueue::linear", page = pages, fetched, kept = out.len(), "fetched issues page");
            if out.len() >= limit || !page.page_info.has_next_page || fetched == 0 {
                break;
            }
            match page.page_info.end_cursor {
                Some(c) => after = Some(c),
                None => break,
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// One issue by UUID or identifier (`ENG-123`). `Ok(None)` if it does not
    /// exist (or was deleted / is not visible to the key).
    pub async fn get_issue(&self, id: &str) -> Result<Option<LinearIssue>> {
        let query = format!("query($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} }} }}");
        let data = match self.graphql(&query, serde_json::json!({ "id": id })).await {
            Ok(d) => d,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        match data.get("issue") {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(raw) => {
                let raw: RawIssue = serde_json::from_value(raw.clone()).context("parse issue")?;
                Ok(Some(raw.into()))
            }
        }
    }

    /// Move an issue to the workflow state called `state_name` in its team
    /// (case-insensitive). Errors list the valid state names if none matches.
    pub async fn set_state(&self, issue_id: &str, state_name: &str) -> Result<()> {
        let issue = self.get_issue(issue_id).await?.ok_or_else(|| anyhow!("Linear issue {issue_id} not found"))?;
        self.set_state_in_team(&issue.id, &issue.identifier, &issue.team_key, state_name).await
    }

    /// [`Self::set_state`] for an issue whose UUID (`issue_id`) and team are
    /// already known, saving the issue lookup. `identifier` is only used in
    /// messages.
    pub async fn set_state_in_team(&self, issue_id: &str, identifier: &str, team_key: &str, state_name: &str) -> Result<()> {
        let states = self.workflow_states(team_key).await?;
        let wanted = state_name.trim();
        let state = states.iter().find(|s| s.name.trim().eq_ignore_ascii_case(wanted)).ok_or_else(|| {
            let names: Vec<&str> = states.iter().map(|s| s.name.as_str()).collect();
            anyhow!("team {team_key} has no workflow state named `{wanted}` (valid: {})", names.join(", "))
        })?;
        let mutation =
            "mutation($id: String!, $stateId: String!) { issueUpdate(id: $id, input: { stateId: $stateId }) { success } }";
        let data = self.graphql(mutation, serde_json::json!({ "id": issue_id, "stateId": state.id })).await?;
        if data.pointer("/issueUpdate/success").and_then(|v| v.as_bool()) != Some(true) {
            bail!("Linear did not confirm the state change of {identifier} to `{}`", state.name);
        }
        debug!(target: "powerqueue::linear", issue = %identifier, state = %state.name, "issue state updated");
        Ok(())
    }

    /// Sub-issues and `blocked by` issues of the issues with these UUIDs,
    /// keyed by UUID. Queries 10 issues at a time and
    /// follows every truncated connection to its last page, so the lists
    /// are complete. Issues Linear does not return are absent from the map.
    pub async fn fetch_dependencies(&self, ids: &[String]) -> Result<std::collections::HashMap<String, IssueDependencies>> {
        let query = format!(
            "query($ids: [ID!], $first: Int) {{ issues(filter: {{ id: {{ in: $ids }} }}, first: $first) {{ nodes {{ id \
               children(first: {DEPENDENCY_PAGE}) {{ nodes {{ {CHILD_NODE} }} pageInfo {{ hasNextPage endCursor }} }} \
               inverseRelations(first: {DEPENDENCY_PAGE}) {{ nodes {{ {RELATION_NODE} }} pageInfo {{ hasNextPage endCursor }} }} \
             }} }} }}"
        );
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        for batch in ids.chunks(DEPENDENCY_BATCH) {
            let data = self.graphql(&query, serde_json::json!({ "ids": batch, "first": batch.len() })).await?;
            let nodes = data.pointer("/issues/nodes").cloned().ok_or_else(|| anyhow!("issues missing from response"))?;
            let nodes: Vec<RawDependencies> = serde_json::from_value(nodes).context("parse issue dependencies")?;
            for raw in nodes {
                let id = raw.id.clone();
                let deps = self.complete_dependencies(raw).await?;
                out.insert(id, deps);
            }
        }
        Ok(out)
    }

    /// The parent issue `id` (UUID or identifier) with *all* its sub-issues;
    /// `Ok(None)` if it does not exist or is not visible to the key.
    pub async fn parent_status(&self, id: &str) -> Result<Option<ParentStatus>> {
        let query = format!(
            "query($id: String!) {{ issue(id: $id) {{ id identifier state {{ type }} team {{ key }} \
               children(first: {DEPENDENCY_PAGE}) {{ nodes {{ {CHILD_NODE} }} pageInfo {{ hasNextPage endCursor }} }} }} }}"
        );
        let data = match self.graphql(&query, serde_json::json!({ "id": id })).await {
            Ok(d) => d,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let raw = match data.get("issue") {
            None | Some(serde_json::Value::Null) => return Ok(None),
            Some(raw) => serde_json::from_value::<RawDependencies>(raw.clone()).context("parse parent issue")?,
        };
        let (identifier, state_type, team_key) = (
            raw.identifier.clone().unwrap_or_else(|| id.to_string()),
            raw.state.as_ref().map(|s| s.kind.clone()).unwrap_or_default(),
            raw.team.as_ref().map(|t| t.key.clone()).unwrap_or_default(),
        );
        let issue_id = raw.id.clone();
        let deps = self.complete_dependencies(raw).await?;
        Ok(Some(ParentStatus { id: issue_id, identifier, state_type, team_key, children: deps.children }))
    }

    /// Turn a dependency node into lists, fetching the remaining pages of
    /// any connection Linear truncated.
    async fn complete_dependencies(&self, raw: RawDependencies) -> Result<IssueDependencies> {
        let mut deps = IssueDependencies::default();
        if let Some(conn) = raw.children {
            let mut after = conn.next_cursor();
            deps.children.extend(conn.nodes.into_iter().map(LinkedIssue::from));
            while let Some(cursor) = after {
                let page: RawConnection<RawLinked> = self.dependency_page(&raw.id, "children", CHILD_NODE, &cursor).await?;
                after = page.next_cursor();
                deps.children.extend(page.nodes.into_iter().map(LinkedIssue::from));
            }
        }
        if let Some(conn) = raw.inverse_relations {
            let mut after = conn.next_cursor();
            deps.blocked_by.extend(blockers_of(conn.nodes));
            while let Some(cursor) = after {
                let page: RawConnection<RawRelation> =
                    self.dependency_page(&raw.id, "inverseRelations", RELATION_NODE, &cursor).await?;
                after = page.next_cursor();
                deps.blocked_by.extend(blockers_of(page.nodes));
            }
        }
        Ok(deps)
    }

    /// The page after `cursor` of the connection `field` of issue `id`.
    async fn dependency_page<T: serde::de::DeserializeOwned>(
        &self,
        id: &str,
        field: &str,
        node: &str,
        cursor: &str,
    ) -> Result<RawConnection<T>> {
        let query = format!(
            "query($id: String!, $after: String) {{ issue(id: $id) {{ \
               {field}(first: {DEPENDENCY_PAGE}, after: $after) {{ nodes {{ {node} }} pageInfo {{ hasNextPage endCursor }} }} }} }}"
        );
        let data = self.graphql(&query, serde_json::json!({ "id": id, "after": cursor })).await?;
        let conn = data.get("issue").and_then(|i| i.get(field)).cloned().ok_or_else(|| anyhow!("{field} of {id} missing"))?;
        serde_json::from_value(conn).with_context(|| format!("parse {field} page of {id}"))
    }

    /// True if a GitHub pull request attached to the issue (`id` is a UUID
    /// or identifier) is merged, according to the attachment metadata the
    /// Linear GitHub integration keeps (see [`attachments_show_merged_pr`]).
    /// `Ok(false)` for a missing issue.
    pub async fn pr_merged(&self, id: &str) -> Result<bool> {
        let query = "query($id: String!) { issue(id: $id) { attachments(first: 20) { nodes { url sourceType metadata } } } }";
        let data = match self.graphql(query, serde_json::json!({ "id": id })).await {
            Ok(d) => d,
            Err(e) if is_not_found(&e) => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("fetch attachments of {id}")),
        };
        Ok(data.pointer("/issue/attachments/nodes").is_some_and(attachments_show_merged_pr))
    }

    /// Post a Markdown comment on an issue.
    pub async fn comment(&self, issue_id: &str, body: &str) -> Result<()> {
        self.post_comment(issue_id, body).await.map(|_| ())
    }

    /// Post a Markdown comment on an issue and return the created comment
    /// (`None` when Linear confirms without returning it). Fails when
    /// Linear does not confirm the comment.
    pub async fn post_comment(&self, issue_id: &str, body: &str) -> Result<Option<IssueComment>> {
        let mutation = "mutation($issueId: String!, $body: String!) { commentCreate(input: { issueId: $issueId, body: $body }) { success comment { id body createdAt user { displayName name } } } }";
        let data = self.graphql(mutation, serde_json::json!({ "issueId": issue_id, "body": body })).await?;
        if data.pointer("/commentCreate/success").and_then(|v| v.as_bool()) != Some(true) {
            bail!("Linear did not confirm the comment on issue {issue_id}");
        }
        debug!(target: "powerqueue::linear", issue = %issue_id, bytes = body.len(), "comment posted");
        Ok(data.pointer("/commentCreate/comment").and_then(parse_comment))
    }

    /// Comments on an issue created after `since`, oldest first (at most
    /// 50). An issue that no longer exists has none.
    pub async fn comments_since(&self, issue_id: &str, since: DateTime<Utc>) -> Result<Vec<IssueComment>> {
        let query = "query($id: String!, $since: DateTimeOrDuration!) { issue(id: $id) { comments(first: 50, filter: { createdAt: { gt: $since } }) { nodes { id body createdAt user { displayName name } } } } }";
        let since = since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let data = match self.graphql(query, serde_json::json!({ "id": issue_id, "since": since })).await {
            Ok(d) => d,
            Err(e) if is_not_found(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("fetch comments of {issue_id}")),
        };
        let mut comments: Vec<IssueComment> = data
            .pointer("/issue/comments/nodes")
            .and_then(|n| n.as_array())
            .map(|nodes| nodes.iter().filter_map(parse_comment).collect())
            .unwrap_or_default();
        comments.sort_by_key(|c| c.created_at);
        Ok(comments)
    }
}

/// A comment on a Linear issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueComment {
    pub id: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    /// Display name of the author, if Linear reports a user.
    pub author: Option<String>,
}

/// Parse a Linear `Comment` node; `None` without an id or a valid `createdAt`.
fn parse_comment(node: &serde_json::Value) -> Option<IssueComment> {
    let id = node.get("id")?.as_str()?.to_string();
    let created_at = node.get("createdAt")?.as_str()?.parse::<DateTime<Utc>>().ok()?;
    let body = node.get("body").and_then(|b| b.as_str()).unwrap_or_default().to_string();
    let author = node
        .get("user")
        .and_then(|u| u.get("displayName").or_else(|| u.get("name")))
        .and_then(|n| n.as_str())
        .map(str::to_string);
    Some(IssueComment { id, body, created_at, author })
}

/// True if any attachment in `nodes` (Linear `Attachment` objects with
/// `url`, `sourceType`, `metadata`) is a GitHub pull request whose metadata
/// says it is merged (`status: "merged"` or a `mergedAt` timestamp).
pub fn attachments_show_merged_pr(nodes: &serde_json::Value) -> bool {
    let Some(nodes) = nodes.as_array() else { return false };
    nodes.iter().any(|a| {
        let url = a.get("url").and_then(|u| u.as_str()).unwrap_or("");
        let source = a.get("sourceType").and_then(|s| s.as_str()).unwrap_or("");
        let is_pr = (url.contains("github.com") && url.contains("/pull/")) || source.to_ascii_lowercase().contains("github");
        let meta = a.get("metadata");
        let status_merged =
            meta.and_then(|m| m.get("status")).and_then(|s| s.as_str()).is_some_and(|s| s.eq_ignore_ascii_case("merged"));
        let merged_at = meta.and_then(|m| m.get("mergedAt")).is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()));
        is_pr && (status_merged || merged_at)
    })
}

/// Delay before retry `attempt` (1-based): `Retry-After` if given, else
/// exponential backoff from [`BASE_BACKOFF`], capped at [`MAX_BACKOFF`].
fn backoff_for(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let wait = retry_after.unwrap_or_else(|| BASE_BACKOFF * 2u32.saturating_pow(attempt.saturating_sub(1)));
    wait.min(MAX_BACKOFF)
}

/// Best-effort operation name for logs: the first field after `query`/`mutation`.
fn operation_name(query: &str) -> String {
    let body = query.split('{').nth(1).unwrap_or("");
    body.split(|c: char| !c.is_alphanumeric() && c != '_').find(|s| !s.is_empty()).unwrap_or("graphql").to_string()
}

/// Linear reports a missing entity as a GraphQL error rather than `null`.
fn is_not_found(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("entity not found") || msg.contains("could not find")
}

fn excerpt(text: &str) -> String {
    const MAX: usize = 300;
    let t = text.trim();
    if t.chars().count() > MAX { format!("{}…", t.chars().take(MAX).collect::<String>()) } else { t.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(labels: &[&str]) -> LinearIssue {
        LinearIssue {
            id: "u".into(),
            identifier: "ENG-1".into(),
            title: "t".into(),
            description: String::new(),
            url: String::new(),
            priority: 0,
            estimate: None,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            state_name: "Todo".into(),
            state_type: "unstarted".into(),
            team_key: "ENG".into(),
            project: None,
            assignee_id: None,
            cycle: None,
            blocked_by: Vec::new(),
            children: Vec::new(),
            parent: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// Recorded shape of a node of the dependency query (see
    /// `tests/fixtures/linear/issue_with_relations.json`).
    const RELATIONS_FIXTURE: &str = include_str!("../../tests/fixtures/linear/issue_with_relations.json");

    #[test]
    fn dependency_node_parses_blockers_and_children() {
        let raw: RawDependencies = serde_json::from_str(RELATIONS_FIXTURE).unwrap();
        assert!(raw.children.as_ref().unwrap().next_cursor().is_none());
        let blockers: Vec<LinkedIssue> = blockers_of(raw.inverse_relations.unwrap().nodes).collect();
        let keys: Vec<(&str, &str)> = blockers.iter().map(|b| (b.key.as_str(), b.state_type.as_str())).collect();
        assert_eq!(keys, vec![("AVS-1713", "completed"), ("AVS-1712", "started")], "only `blocks` relations count");
        assert!(blockers.iter().all(|b| !b.pr_merged), "merge status comes from a separate lookup");
        assert_eq!(blockers[1].title, "Second blocker");
        let children: Vec<LinkedIssue> = raw.children.unwrap().nodes.into_iter().map(LinkedIssue::from).collect();
        assert_eq!(children.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec!["AVS-1720", "AVS-1721"]);
        assert!(children[0].is_closed() && !children[1].is_closed());
    }

    #[test]
    fn issue_query_stays_cheap() {
        assert!(ISSUE_FIELDS.contains("parent { identifier }"));
        for nested in ["children", "inverseRelations", "relations"] {
            assert!(!ISSUE_FIELDS.contains(nested), "{nested} would multiply the page's complexity");
        }
        let raw: RawIssue = serde_json::from_value(serde_json::json!({
            "id": "abc", "identifier": "ENG-9", "title": "T", "url": "https://x", "parent": { "identifier": "ENG-1" },
            "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-02T00:00:00.000Z"
        }))
        .unwrap();
        assert_eq!(LinearIssue::from(raw).parent.as_deref(), Some("ENG-1"));
    }

    #[test]
    fn merged_pr_detection_from_attachments() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/linear/attachments.json")).unwrap();
        let nodes = fixture.pointer("/data/issue/attachments/nodes").unwrap();
        assert!(attachments_show_merged_pr(nodes));
        let open_only = serde_json::json!([
            { "url": "https://github.com/o/r/pull/1", "sourceType": "github", "metadata": { "status": "open" } },
            { "url": "https://example.com/doc", "sourceType": "api", "metadata": { "status": "merged" } }
        ]);
        assert!(!attachments_show_merged_pr(&open_only), "a non-GitHub attachment never counts");
        let merged_at = serde_json::json!([
            { "url": "https://github.com/o/r/pull/2", "sourceType": null, "metadata": { "mergedAt": "2026-10-01T00:00:00Z" } }
        ]);
        assert!(attachments_show_merged_pr(&merged_at));
        assert!(!attachments_show_merged_pr(&serde_json::json!([])));
        assert!(!attachments_show_merged_pr(&serde_json::Value::Null));
    }

    #[test]
    fn server_filter_covers_cycle_scopes_and_projects() {
        let base = IssueFilter { team_keys: vec!["ENG".into()], state_names: vec!["Todo".into()], ..Default::default() };
        let f = serde_json::Value::Object(base.server_filter());
        assert_eq!(f["team"]["key"]["in"], serde_json::json!(["ENG"]));
        assert!(f.get("cycle").is_none() && f.get("or").is_none() && f.get("project").is_none(), "{f}");

        let f = serde_json::Value::Object(IssueFilter { cycle: CycleScope::Active, ..base.clone() }.server_filter());
        assert_eq!(f["cycle"], serde_json::json!({ "isActive": { "eq": true } }));
        let f = serde_json::Value::Object(IssueFilter { cycle: CycleScope::Next, ..base.clone() }.server_filter());
        assert_eq!(f["cycle"], serde_json::json!({ "isNext": { "eq": true } }));
        let f = serde_json::Value::Object(IssueFilter { cycle: CycleScope::None, ..base.clone() }.server_filter());
        assert_eq!(f["cycle"], serde_json::json!({ "null": true }));
        let f = serde_json::Value::Object(IssueFilter { cycle: CycleScope::ActiveOrNext, ..base.clone() }.server_filter());
        assert!(f.get("cycle").is_none());
        assert_eq!(
            f["or"],
            serde_json::json!([{ "cycle": { "isActive": { "eq": true } } }, { "cycle": { "isNext": { "eq": true } } }])
        );
        // Teams and states still apply next to the `or`.
        assert_eq!(f["state"]["name"]["in"], serde_json::json!(["Todo"]));

        let f =
            serde_json::Value::Object(IssueFilter { projects: vec!["Launch".into(), "Platform".into()], ..base }.server_filter());
        assert_eq!(f["project"]["name"]["in"], serde_json::json!(["Launch", "Platform"]));
    }

    #[test]
    fn filter_from_config_reads_cycle_and_projects() {
        let mut cfg = crate::config::LinearConfig { cycle: "current".into(), ..Default::default() };
        cfg.projects = vec![" Launch ".into(), "".into()];
        let f = IssueFilter::from_config(&cfg);
        assert_eq!(f.cycle, CycleScope::Active);
        assert_eq!(f.projects, vec!["Launch".to_string()]);
        cfg.cycle = "bogus".into();
        assert_eq!(IssueFilter::from_config(&cfg).cycle, CycleScope::Any, "unknown values never narrow the queue");
    }

    #[test]
    fn cycle_status_from_flags() {
        assert_eq!(CycleStatus::from_flags(true, false, false, false), CycleStatus::Active);
        assert_eq!(CycleStatus::from_flags(false, true, false, true), CycleStatus::Next);
        assert_eq!(CycleStatus::from_flags(false, false, true, false), CycleStatus::Past);
        assert_eq!(CycleStatus::from_flags(false, false, false, true), CycleStatus::Future);
        assert_eq!(CycleStatus::from_flags(false, false, false, false), CycleStatus::Other);
        assert_eq!(CycleStatus::Active.as_str(), "active");
    }

    #[test]
    fn raw_issue_parses_cycle_and_tolerates_missing_cycle_fields() {
        let base = serde_json::json!({
            "id": "abc", "identifier": "ENG-9", "title": "T", "url": "https://x",
            "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-02T00:00:00.000Z"
        });
        let mut with_cycle = base.clone();
        with_cycle["cycle"] = serde_json::json!({ "number": 14.0, "name": null, "isActive": false, "isNext": true, "isPast": false, "isFuture": true });
        let raw: RawIssue = serde_json::from_value(with_cycle).unwrap();
        let issue = LinearIssue::from(raw);
        let cycle = issue.cycle.clone().expect("cycle");
        assert_eq!(cycle.number, 14);
        assert_eq!(cycle.name, None);
        assert_eq!(cycle.status, CycleStatus::Next);
        assert_eq!(issue.cycle_status(), Some("next"));

        // A cycle object with only a number (older API shapes) and an explicit null.
        let mut bare = base.clone();
        bare["cycle"] = serde_json::json!({ "number": 3, "name": "Sprint 3" });
        let issue = LinearIssue::from(serde_json::from_value::<RawIssue>(bare).unwrap());
        assert_eq!(issue.cycle, Some(CycleInfo { number: 3, name: Some("Sprint 3".into()), status: CycleStatus::Other }));
        let mut null = base.clone();
        null["cycle"] = serde_json::Value::Null;
        let issue = LinearIssue::from(serde_json::from_value::<RawIssue>(null).unwrap());
        assert_eq!(issue.cycle, None);
        assert_eq!(issue.cycle_status(), None);
        let issue = LinearIssue::from(serde_json::from_value::<RawIssue>(base).unwrap());
        assert_eq!(issue.cycle, None, "a missing `cycle` key is fine too");
    }

    #[test]
    fn label_filter_is_case_insensitive() {
        let f =
            IssueFilter { required_labels: vec!["Agent".into()], excluded_labels: vec!["NO-AGENT".into()], ..Default::default() };
        assert!(f.labels_allow(&issue(&["agent"])));
        assert!(!f.labels_allow(&issue(&["agent", "no-agent"])));
        assert!(!f.labels_allow(&issue(&["other"])));
        let none = IssueFilter::default();
        assert!(none.labels_allow(&issue(&[])));
    }

    #[test]
    fn raw_issue_converts_with_nulls() {
        let raw: RawIssue = serde_json::from_value(serde_json::json!({
            "id": "abc", "identifier": "ENG-9", "title": "T", "description": null, "url": "https://x",
            "priority": 2, "estimate": null, "labels": { "nodes": [{"name": "bug"}] },
            "state": { "name": "Todo", "type": "unstarted" }, "team": { "key": "ENG" },
            "project": null, "assignee": null,
            "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-02T00:00:00.000Z"
        }))
        .unwrap();
        let issue = LinearIssue::from(raw);
        assert_eq!(issue.description, "");
        assert_eq!(issue.priority, 2);
        assert_eq!(issue.labels, vec!["bug".to_string()]);
        assert_eq!(issue.team_key, "ENG");
        assert!(issue.project.is_none());
    }

    #[test]
    fn child_labels_are_qualified_with_their_parent() {
        let raw: RawIssue = serde_json::from_value(serde_json::json!({
            "id": "abc", "identifier": "ENG-9", "title": "T", "url": "https://x",
            "labels": { "nodes": [
                {"name": "fable", "parent": {"name": "model"}},
                {"name": "fable", "parent": null},
                {"name": "bug"}
            ] },
            "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-02T00:00:00.000Z"
        }))
        .unwrap();
        let issue = LinearIssue::from(raw);
        assert_eq!(issue.labels, vec!["model/fable".to_string(), "fable".to_string(), "bug".to_string()]);
        assert!(ISSUE_FIELDS.contains("labels { nodes { name parent { name } } }"));
    }

    #[test]
    fn label_filter_understands_qualified_labels() {
        let f = IssueFilter { excluded_labels: vec!["skip".into()], ..Default::default() };
        assert!(!f.labels_allow(&issue(&["flags/skip"])), "unqualified filter still sees the child label");
        let f = IssueFilter { required_labels: vec!["model/fable".into()], ..Default::default() };
        assert!(f.labels_allow(&issue(&["Model/Fable"])));
        assert!(!f.labels_allow(&issue(&["fable"])), "qualified filter ignores a loose label");
        assert!(!f.labels_allow(&issue(&["other/fable"])));
    }

    #[test]
    fn backoff_honours_retry_after_and_caps() {
        assert_eq!(backoff_for(1, None), Duration::from_millis(500));
        assert_eq!(backoff_for(2, None), Duration::from_millis(1000));
        assert_eq!(backoff_for(3, None), Duration::from_millis(2000));
        assert_eq!(backoff_for(1, Some(Duration::from_secs(3))), Duration::from_secs(3));
        assert_eq!(backoff_for(1, Some(Duration::from_secs(600))), MAX_BACKOFF);
    }

    #[test]
    fn operation_name_extraction() {
        assert_eq!(operation_name("query { viewer { id } }"), "viewer");
        assert_eq!(operation_name("mutation($id: String!) { issueUpdate(id: $id) { success } }"), "issueUpdate");
        assert_eq!(operation_name("nonsense"), "graphql");
    }
}
