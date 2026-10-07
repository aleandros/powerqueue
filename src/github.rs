//! GitHub pull requests through the `gh` CLI (the PR watcher's eyes).
//!
//! One `gh api graphql` call per poll returns everything the watcher decides
//! on: state, mergeability, labels, merge queue membership and removals,
//! required checks of the head commit and review threads. Parsing is a pure function ([`parse_pr_status`]) so the
//! decision table is tested without `gh`.

use std::fmt;
use std::process::{Command, Stdio};
use std::str::FromStr;

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A pull request named by its URL (`https://github.com/<owner>/<repo>/pull/<n>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl FromStr for PrRef {
    type Err = String;

    /// Accepts `https://github.com/o/r/pull/7` (scheme optional, trailing
    /// path such as `/files` or a fragment ignored). Any other host or shape
    /// is an error naming what was expected.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let rest = trimmed.strip_prefix("https://").or_else(|| trimmed.strip_prefix("http://")).unwrap_or(trimmed);
        let rest = rest.split(['#', '?']).next().unwrap_or_default();
        let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
        let bad =
            || format!("`{trimmed}` is not a GitHub pull request URL (expected https://github.com/<owner>/<repo>/pull/<number>)");
        match parts.as_slice() {
            [host, owner, repo, "pull", number, ..] if host.eq_ignore_ascii_case("github.com") => {
                let number = number.parse::<u64>().map_err(|_| bad())?;
                Ok(PrRef { owner: owner.to_string(), repo: repo.to_string(), number })
            }
            _ => Err(bad()),
        }
    }
}

impl fmt::Display for PrRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

/// One check (check run or commit status) of the PR's head commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrCheck {
    pub name: String,
    /// Conclusion of a finished check run (`SUCCESS`, `FAILURE`, ...) or the
    /// state of a commit status; empty while a check run is still going.
    pub conclusion: String,
    /// Branch protection requires it.
    pub required: bool,
}

impl PrCheck {
    /// True for a finished check that did not pass.
    pub fn failed(&self) -> bool {
        matches!(self.conclusion.as_str(), "FAILURE" | "ERROR" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED")
    }
}

/// One review thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrThread {
    pub resolved: bool,
    /// Time of the thread's newest comment.
    pub last_comment_at: Option<DateTime<Utc>>,
}

/// The PR left the merge queue (a `RemovedFromMergeQueueEvent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueRemoval {
    pub at: DateTime<Utc>,
    /// GitHub's reason: `merged`, `manual` (a person dequeued it), or why
    /// the queue dropped it (failed checks, a conflict, ...).
    pub reason: String,
}

impl QueueRemoval {
    /// True when the queue itself dropped the PR, as opposed to merging it
    /// or a person taking it out.
    pub fn dropped(&self) -> bool {
        let reason = self.reason.trim();
        !(reason.eq_ignore_ascii_case("merged") || reason.eq_ignore_ascii_case("manual"))
    }
}

/// What GitHub says about a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatus {
    pub number: u64,
    /// `OPEN`, `MERGED` or `CLOSED`.
    pub state: String,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN` (still being computed).
    pub mergeable: String,
    /// `CLEAN`, `BLOCKED`, `BEHIND`, `DIRTY`, `UNSTABLE`, `HAS_HOOKS`, `UNKNOWN`, ...
    pub merge_state_status: String,
    pub auto_merge: bool,
    /// The PR is in the repository's merge queue.
    #[serde(default)]
    pub in_merge_queue: bool,
    /// The newest merge queue removals, oldest first.
    #[serde(default)]
    pub queue_removals: Vec<QueueRemoval>,
    pub labels: Vec<String>,
    pub head_oid: Option<String>,
    pub checks: Vec<PrCheck>,
    pub threads: Vec<PrThread>,
}

impl PrStatus {
    /// Names of required checks that failed, sorted and de-duplicated.
    pub fn failed_required_checks(&self) -> Vec<String> {
        let mut names: Vec<String> = self.checks.iter().filter(|c| c.required && c.failed()).map(|c| c.name.clone()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// Unresolved threads with a comment newer than `since`.
    pub fn new_unresolved_threads(&self, since: DateTime<Utc>) -> usize {
        self.threads.iter().filter(|t| !t.resolved && t.last_comment_at.is_some_and(|at| at > since)).count()
    }

    /// Why the merge queue dropped this PR after `since`, when it is still
    /// open and neither queued nor armed again: the reason of the newest
    /// removal, if that removal was not a merge or a person's.
    pub fn dropped_from_queue(&self, since: DateTime<Utc>) -> Option<&str> {
        if self.state != "OPEN" || self.in_merge_queue || self.auto_merge {
            return None;
        }
        let last = self.queue_removals.iter().max_by_key(|r| r.at)?;
        (last.at > since && last.dropped()).then_some(last.reason.as_str())
    }

    /// True if the PR carries `label` (case-insensitive).
    pub fn has_label(&self, label: &str) -> bool {
        let label = label.trim();
        !label.is_empty() && self.labels.iter().any(|l| l.eq_ignore_ascii_case(label))
    }
}

/// GraphQL query run by [`Gh::pr_status`].
pub const PR_STATUS_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number state mergeable mergeStateStatus headRefOid isInMergeQueue
      autoMergeRequest { enabledAt }
      timelineItems(last: 5, itemTypes: [REMOVED_FROM_MERGE_QUEUE_EVENT]) {
        nodes { ... on RemovedFromMergeQueueEvent { createdAt reason } }
      }
      labels(first: 50) { nodes { name } }
      reviewThreads(first: 100) { nodes { isResolved comments(last: 1) { nodes { createdAt } } } }
      commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 100) { nodes {
        __typename
        ... on CheckRun { name status conclusion isRequired(pullRequestNumber: $number) }
        ... on StatusContext { context state isRequired(pullRequestNumber: $number) }
      } } } } } }
    }
  }
}"#;

/// Parse the JSON `gh api graphql` printed for [`PR_STATUS_QUERY`]. Fails
/// on GraphQL errors or a missing pull request, naming what was wrong.
pub fn parse_pr_status(text: &str) -> Result<PrStatus> {
    let v: serde_json::Value = serde_json::from_str(text).context("gh printed something that is not JSON")?;
    if let Some(errors) = v.get("errors").and_then(|e| e.as_array()).filter(|e| !e.is_empty()) {
        let messages: Vec<&str> = errors.iter().filter_map(|e| e["message"].as_str()).collect();
        bail!("GitHub GraphQL error: {}", messages.join("; "));
    }
    let pr = &v["data"]["repository"]["pullRequest"];
    if pr.is_null() {
        bail!("GitHub returned no pull request (wrong URL, or gh lacks access to the repository)");
    }
    let s = |val: &serde_json::Value| val.as_str().unwrap_or_default().to_string();
    let nodes = |val: &serde_json::Value| val["nodes"].as_array().cloned().unwrap_or_default();
    let labels = nodes(&pr["labels"]).iter().map(|l| s(&l["name"])).filter(|l| !l.is_empty()).collect();
    let threads = nodes(&pr["reviewThreads"])
        .iter()
        .map(|t| PrThread {
            resolved: t["isResolved"].as_bool().unwrap_or(false),
            last_comment_at: nodes(&t["comments"])
                .iter()
                .filter_map(|c| c["createdAt"].as_str())
                .filter_map(|at| DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&Utc))
                .max(),
        })
        .collect();
    let queue_removals = nodes(&pr["timelineItems"])
        .iter()
        .filter_map(|e| {
            let at = DateTime::parse_from_rfc3339(e["createdAt"].as_str()?).ok()?.with_timezone(&Utc);
            Some(QueueRemoval { at, reason: s(&e["reason"]) })
        })
        .collect();
    let mut checks = Vec::new();
    for commit in nodes(&pr["commits"]) {
        for ctx in nodes(&commit["commit"]["statusCheckRollup"]["contexts"]) {
            let required = ctx["isRequired"].as_bool().unwrap_or(false);
            let check = if ctx["__typename"] == "StatusContext" {
                PrCheck { name: s(&ctx["context"]), conclusion: s(&ctx["state"]), required }
            } else {
                PrCheck { name: s(&ctx["name"]), conclusion: s(&ctx["conclusion"]), required }
            };
            checks.push(check);
        }
    }
    Ok(PrStatus {
        number: pr["number"].as_u64().unwrap_or_default(),
        state: s(&pr["state"]),
        mergeable: s(&pr["mergeable"]),
        merge_state_status: s(&pr["mergeStateStatus"]),
        auto_merge: !pr["autoMergeRequest"].is_null(),
        in_merge_queue: pr["isInMergeQueue"].as_bool().unwrap_or(false),
        queue_removals,
        labels,
        head_oid: pr["headRefOid"].as_str().map(str::to_string),
        checks,
        threads,
    })
}

/// The `gh` CLI.
#[derive(Debug, Clone)]
pub struct Gh {
    binary: String,
}

impl Gh {
    pub fn new(binary: &str) -> Self {
        Self { binary: binary.to_string() }
    }

    /// Ask GitHub for a PR's status (`gh api graphql`). Fails when `gh` is
    /// missing, not authenticated or the query fails; the error carries
    /// gh's stderr.
    pub fn pr_status(&self, pr: &PrRef) -> Result<PrStatus> {
        let number = pr.number.to_string();
        let args = [
            "api",
            "graphql",
            "-f",
            &format!("query={PR_STATUS_QUERY}"),
            "-f",
            &format!("owner={}", pr.owner),
            "-f",
            &format!("name={}", pr.repo),
            "-F",
            &format!("number={number}"),
        ];
        let out = Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .env("GH_PROMPT_DISABLED", "1")
            .output()
            .with_context(|| format!("cannot run `{} api graphql` for {pr} (is the GitHub CLI installed?)", self.binary))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            // gh prints GraphQL errors as JSON on stdout and exits 1.
            if let Err(e) = parse_pr_status(&stdout) {
                return Err(anyhow!(
                    "`{} api graphql` for {pr} failed ({}): {}",
                    self.binary,
                    out.status,
                    if stderr.is_empty() { format!("{e:#}") } else { stderr }
                ));
            }
        }
        parse_pr_status(&stdout).with_context(|| format!("read the status of {pr}"))
    }

    /// URL of an open pull request whose head is `branch`, in the GitHub
    /// repository of the git checkout at `repo_dir` (`gh pr list --head`).
    /// `Ok(None)` when there is none. Fails when `gh` is missing, not
    /// authenticated or cannot tell the repository; the error carries stderr.
    pub fn open_pr_for_branch(&self, repo_dir: &std::path::Path, branch: &str) -> Result<Option<String>> {
        let out = Command::new(&self.binary)
            .args(["pr", "list", "--head", branch, "--state", "open", "--json", "url", "--limit", "1"])
            .current_dir(repo_dir)
            .stdin(Stdio::null())
            .env("GH_PROMPT_DISABLED", "1")
            .output()
            .with_context(|| format!("cannot run `{} pr list --head {branch}` (is the GitHub CLI installed?)", self.binary))?;
        if !out.status.success() {
            return Err(anyhow!(
                "`{} pr list --head {branch}` failed in {} ({}): {}",
                self.binary,
                repo_dir.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        parse_pr_list(&String::from_utf8_lossy(&out.stdout))
    }
}

/// First PR URL in the JSON `gh pr list --json url` printed; `None` for an
/// empty list. Fails on output that is not that JSON.
pub fn parse_pr_list(text: &str) -> Result<Option<String>> {
    let v: serde_json::Value = serde_json::from_str(text).context("gh pr list printed something that is not JSON")?;
    let list = v.as_array().ok_or_else(|| anyhow!("gh pr list did not print a JSON array"))?;
    Ok(list.iter().filter_map(|pr| pr["url"].as_str()).map(str::to_string).find(|u| !u.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_list_yields_the_first_url() {
        assert_eq!(
            parse_pr_list(r#"[{"url":"https://github.com/o/r/pull/1079"}]"#).unwrap().as_deref(),
            Some("https://github.com/o/r/pull/1079")
        );
        assert_eq!(parse_pr_list("[]").unwrap(), None);
        assert!(parse_pr_list("nope").is_err());
    }

    fn sample(state: &str, mergeable: &str) -> serde_json::Value {
        serde_json::json!({ "data": { "repository": { "pullRequest": {
            "number": 7, "state": state, "mergeable": mergeable, "mergeStateStatus": "BLOCKED", "headRefOid": "abc",
            "autoMergeRequest": { "enabledAt": "2026-10-06T10:00:00Z" }, "isInMergeQueue": false,
            "timelineItems": { "nodes": [
                { "createdAt": "2026-10-06T09:00:00Z", "reason": "manual" },
                { "createdAt": "2026-10-06T11:00:00Z", "reason": "failed checks" }
            ] },
            "labels": { "nodes": [{ "name": "merge/hold" }] },
            "reviewThreads": { "nodes": [
                { "isResolved": false, "comments": { "nodes": [{ "createdAt": "2026-10-06T12:00:00Z" }] } },
                { "isResolved": true, "comments": { "nodes": [{ "createdAt": "2026-10-06T13:00:00Z" }] } },
                { "isResolved": false, "comments": { "nodes": [{ "createdAt": "2026-10-06T08:00:00Z" }] } }
            ] },
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "contexts": { "nodes": [
                { "__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "FAILURE", "isRequired": true },
                { "__typename": "CheckRun", "name": "lint", "status": "COMPLETED", "conclusion": "FAILURE", "isRequired": false },
                { "__typename": "CheckRun", "name": "build", "status": "IN_PROGRESS", "conclusion": null, "isRequired": true },
                { "__typename": "StatusContext", "context": "ci/legacy", "state": "ERROR", "isRequired": true }
            ] } } } }] }
        } } } })
    }

    #[test]
    fn parses_pr_urls() {
        let pr: PrRef = "https://github.com/aleandros/powerqueue/pull/12".parse().unwrap();
        assert_eq!(pr, PrRef { owner: "aleandros".into(), repo: "powerqueue".into(), number: 12 });
        assert_eq!(pr.to_string(), "aleandros/powerqueue#12");
        let pr: PrRef = "github.com/o/r/pull/3/files#diff".parse().unwrap();
        assert_eq!(pr.number, 3);
        for bad in ["https://gitlab.com/o/r/pull/1", "https://github.com/o/r/issues/1", "https://github.com/o/r/pull/x", "7"] {
            assert!(bad.parse::<PrRef>().is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_status_checks_and_threads() {
        let st = parse_pr_status(&sample("OPEN", "MERGEABLE").to_string()).unwrap();
        assert_eq!((st.number, st.state.as_str(), st.mergeable.as_str()), (7, "OPEN", "MERGEABLE"));
        assert!(st.auto_merge);
        assert!(st.has_label("MERGE/HOLD") && !st.has_label(""));
        assert_eq!(st.queue_removals.len(), 2);
        assert!(!st.in_merge_queue);
        assert_eq!(st.failed_required_checks(), vec!["ci/legacy".to_string(), "test".to_string()]);
        let since = DateTime::parse_from_rfc3339("2026-10-06T10:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(st.new_unresolved_threads(since), 1, "resolved and older threads do not count");
    }

    #[test]
    fn a_queue_drop_counts_only_after_since_and_while_unqueued() {
        let at = |h: u32| DateTime::parse_from_rfc3339(&format!("2026-10-06T{h:02}:00:00Z")).unwrap().with_timezone(&Utc);
        let mut st = parse_pr_status(&sample("OPEN", "MERGEABLE").to_string()).unwrap();
        assert_eq!(st.dropped_from_queue(at(10)), None, "auto-merge still armed");
        st.auto_merge = false;
        assert_eq!(st.dropped_from_queue(at(10)), Some("failed checks"));
        assert_eq!(st.dropped_from_queue(at(12)), None, "removal before the hand-off");
        st.in_merge_queue = true;
        assert_eq!(st.dropped_from_queue(at(10)), None, "queued again");
        st.in_merge_queue = false;
        st.queue_removals.push(QueueRemoval { at: at(12), reason: "manual".into() });
        assert_eq!(st.dropped_from_queue(at(10)), None, "a person took it out last");
        st.queue_removals.push(QueueRemoval { at: at(13), reason: "MERGED".into() });
        assert_eq!(st.dropped_from_queue(at(10)), None);
    }

    #[test]
    fn reports_graphql_errors_and_missing_prs() {
        let err = parse_pr_status(r#"{"errors":[{"message":"Could not resolve to a Repository"}]}"#).unwrap_err();
        assert!(format!("{err:#}").contains("Could not resolve"), "{err:#}");
        let err = parse_pr_status(r#"{"data":{"repository":{"pullRequest":null}}}"#).unwrap_err();
        assert!(format!("{err:#}").contains("no pull request"), "{err:#}");
        assert!(parse_pr_status("not json").is_err());
    }
}
