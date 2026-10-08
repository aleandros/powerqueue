//! Small GitHub REST client. Failed writes are not automatically retried.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};

use crate::config::GitHubConfig;

/// GitHub requested that API calls pause until this time.
#[derive(Debug, thiserror::Error)]
#[error("GitHub API rate limit; retry after {retry_at}")]
pub struct RateLimited {
    pub retry_at: DateTime<Utc>,
}

/// Issue metadata used by the queue. Pull requests are filtered out by the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubIssue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub html_url: String,
    pub state: String,
    pub labels: Vec<Label>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

/// An issue label returned by GitHub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Label {
    pub name: String,
}

/// Authenticated API client with bounded requests and redirects disabled.
#[derive(Clone)]
pub struct GitHubClient {
    http: reqwest::Client,
    endpoint: Url,
}

impl GitHubClient {
    /// Build a client; fails for invalid endpoints, tokens or HTTP configuration.
    pub fn new(endpoint: &str, token: String) -> Result<Self> {
        use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
        let cfg = GitHubConfig { endpoint: endpoint.to_string(), ..Default::default() };
        let problems = cfg.validate();
        if !problems.is_empty() {
            bail!("{}", problems.join("; "));
        }
        let mut headers = HeaderMap::new();
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}")).context("invalid GitHub token")?;
        authorization.set_sensitive(true);
        headers.insert(AUTHORIZATION, authorization);
        headers.insert("accept", HeaderValue::from_static("application/vnd.github+json"));
        headers.insert("x-github-api-version", HeaderValue::from_static("2022-11-28"));
        let http = reqwest::Client::builder()
            .user_agent(concat!("powerqueue/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("build GitHub HTTP client")?;
        Ok(Self { http, endpoint: Url::parse(endpoint).context("parse GitHub endpoint")? })
    }

    fn url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("GitHub endpoint cannot be a base URL"))?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    fn issue_url(&self, repository: &str, number: Option<u64>, suffix: &[&str]) -> Result<Url> {
        let (owner, repo) = repository.split_once('/').context("github.repository must be owner/repo")?;
        let mut parts = vec!["repos", owner, repo, "issues"];
        let number = number.map(|n| n.to_string());
        if let Some(n) = &number {
            parts.push(n);
        }
        parts.extend_from_slice(suffix);
        self.url(&parts)
    }

    async fn request(&self, method: Method, url: Url, body: Option<serde_json::Value>) -> Result<reqwest::Response> {
        let mut req = self.http.request(method.clone(), url.clone());
        if let Some(body) = body {
            req = req.json(&body);
        }
        let response = req.send().await.with_context(|| format!("GitHub {method} {}", url.path()))?;
        let status = response.status();
        let headers = response.headers();
        if status == StatusCode::TOO_MANY_REQUESTS
            || (status == StatusCode::FORBIDDEN
                && (headers.contains_key("retry-after") || headers.get("x-ratelimit-remaining").is_some_and(|v| v == "0")))
        {
            let seconds = headers.get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
            let reset = headers
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i64>().ok())
                .and_then(|v| DateTime::from_timestamp(v, 0));
            let now = Utc::now();
            let retry_at = seconds
                .and_then(Duration::try_seconds)
                .and_then(|d| now.checked_add_signed(d))
                .into_iter()
                .chain(reset)
                .max()
                .unwrap_or(now + Duration::seconds(60))
                .max(now + Duration::seconds(1));
            return Err(RateLimited { retry_at }.into());
        }
        if !status.is_success() {
            // Do not copy response bodies (which can contain private issue data) to logs.
            bail!(
                "GitHub {method} {} returned HTTP {status}; check repository access, token permissions and API rate limits",
                url.path()
            );
        }
        Ok(response)
    }

    /// Verify repository visibility; errors include missing access and API failures.
    pub async fn test_repository(&self, repository: &str) -> Result<()> {
        let (owner, repo) = repository.split_once('/').context("github.repository must be owner/repo")?;
        self.request(Method::GET, self.url(&["repos", owner, repo])?, None).await?;
        Ok(())
    }

    /// Fetch open issues with all required labels, excluding PRs and excluded labels.
    /// Pagination stops at `max_issues` raw entries. Any failed page fails the fetch.
    pub async fn fetch_issues(&self, cfg: &GitHubConfig) -> Result<Vec<GitHubIssue>> {
        self.fetch_issues_ignoring(cfg, &HashSet::new()).await
    }

    // Completed local tasks can remain open for human review. Do not let them
    // permanently fill the intake cap and starve later issues.
    pub(super) async fn fetch_issues_ignoring(&self, cfg: &GitHubConfig, finished: &HashSet<u64>) -> Result<Vec<GitHubIssue>> {
        let mut out = Vec::new();
        let mut scanned = 0_u64;
        let mut counted = 0_u32;
        let scan_limit = u64::from(cfg.max_issues).saturating_add(finished.len() as u64);
        let mut page = 1;
        while scanned < scan_limit && counted < cfg.max_issues {
            let mut url = self.issue_url(&cfg.repository, None, &[])?;
            url.query_pairs_mut()
                .append_pair("state", "open")
                .append_pair("sort", "created")
                .append_pair("direction", "asc")
                .append_pair("per_page", "100")
                .append_pair("page", &page.to_string());
            if !cfg.required_labels.is_empty() {
                url.query_pairs_mut().append_pair("labels", &cfg.required_labels.join(","));
            }
            if let Some(assignee) = &cfg.assignee {
                url.query_pairs_mut().append_pair("assignee", assignee);
            }
            let issues: Vec<GitHubIssue> =
                self.request(Method::GET, url, None).await?.json().await.context("decode GitHub issue list")?;
            let len = issues.len();
            for issue in issues {
                if scanned >= scan_limit || counted >= cfg.max_issues {
                    break;
                }
                scanned += 1;
                if finished.contains(&issue.number) {
                    continue;
                }
                counted += 1;
                if issue.pull_request.is_none()
                    && issue.state == "open"
                    && cfg.required_labels.iter().all(|label| issue.labels.iter().any(|l| l.name.eq_ignore_ascii_case(label)))
                    && !issue.labels.iter().any(|l| cfg.excluded_labels.iter().any(|x| x.eq_ignore_ascii_case(&l.name)))
                {
                    out.push(issue);
                }
            }
            if len < 100 {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    /// Fetch a tracked issue. A 404 is an error, never evidence of closure.
    pub async fn get_issue(&self, repository: &str, number: u64) -> Result<GitHubIssue> {
        self.request(Method::GET, self.issue_url(repository, Some(number), &[])?, None)
            .await?
            .json()
            .await
            .context("decode GitHub issue")
    }

    /// Add the target lifecycle label and remove only other configured lifecycle labels.
    /// Unrelated labels are preserved. Errors may leave a partially applied transition.
    pub async fn set_label(&self, repository: &str, number: u64, target: &str, managed: &[&str]) -> Result<()> {
        let issue = self.get_issue(repository, number).await?;
        self.request(
            Method::POST,
            self.issue_url(repository, Some(number), &["labels"])?,
            Some(serde_json::json!({"labels": [target]})),
        )
        .await?;
        for label in managed {
            if !label.eq_ignore_ascii_case(target) && issue.labels.iter().any(|l| l.name.eq_ignore_ascii_case(label)) {
                self.request(Method::DELETE, self.issue_url(repository, Some(number), &["labels", label])?, None).await?;
            }
        }
        Ok(())
    }

    /// Close an issue as completed; propagates permission and API failures.
    pub async fn close_issue(&self, repository: &str, number: u64) -> Result<()> {
        self.request(
            Method::PATCH,
            self.issue_url(repository, Some(number), &[])?,
            Some(serde_json::json!({"state": "closed", "state_reason": "completed"})),
        )
        .await?;
        Ok(())
    }

    /// Post one comment. Failed requests are not retried to avoid duplicate comments.
    pub async fn comment(&self, repository: &str, number: u64, body: &str) -> Result<()> {
        self.request(
            Method::POST,
            self.issue_url(repository, Some(number), &["comments"])?,
            Some(serde_json::json!({"body": body})),
        )
        .await?;
        Ok(())
    }
}
