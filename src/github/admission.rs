//! Pure execution approval from GitHub's current issue and event history.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::client::{GitHubIssue, Label};
use crate::config::GitHubConfig;
use crate::domain::GitHubAdmission;

/// Actor on a label event, never the issue author.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub login: String,
}

/// Only events relevant to execution approval are requested.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "__typename")]
pub enum ApprovalEvent {
    LabeledEvent {
        id: String,
        #[serde(rename = "createdAt")]
        at: DateTime<Utc>,
        actor: Option<Actor>,
        label: Label,
    },
    UnlabeledEvent {
        label: Label,
    },
    RenamedTitleEvent {
        #[serde(rename = "createdAt")]
        at: DateTime<Utc>,
    },
}

/// Decide admission without side effects. Events must be complete and in timeline order.
/// Title/body edits (including an edit later reverted) require a newer label application.
pub fn evaluate(
    cfg: &GitHubConfig,
    issue: &GitHubIssue,
    edited: Option<DateTime<Utc>>,
    events: &[ApprovalEvent],
) -> GitHubAdmission {
    if issue.state != "open" || issue.pull_request.is_some() {
        return GitHubAdmission::denied("issue is closed or is a pull request");
    }
    if cfg.required_labels.is_empty() {
        return GitHubAdmission::denied("configure github.required_labels with an explicit execution approval label");
    }
    if issue.labels.iter().any(|l| cfg.excluded_labels.iter().any(|x| l.name.eq_ignore_ascii_case(x))) {
        return GitHubAdmission::denied("issue has an excluded label");
    }
    if let Some(assignee) = &cfg.assignee {
        let matches = match assignee.as_str() {
            "*" => !issue.assignees.is_empty(),
            "none" => issue.assignees.is_empty(),
            login => issue.assignees.iter().any(|a| a.login.eq_ignore_ascii_case(login)),
        };
        if !matches {
            return GitHubAdmission::denied("issue no longer matches github.assignee");
        }
    }
    let last_edit = events
        .iter()
        .filter_map(|e| match e {
            ApprovalEvent::RenamedTitleEvent { at } => Some(*at),
            _ => None,
        })
        .chain(edited)
        .max();
    let mut approvals = Vec::new();
    for required in &cfg.required_labels {
        if !issue.labels.iter().any(|l| l.name.eq_ignore_ascii_case(required)) {
            return GitHubAdmission::denied(format!(
                "approval label {required:?} removed or missing; reapply after reviewing the requested work"
            ));
        }
        let effective = events.iter().rev().find(|e| match e {
            ApprovalEvent::LabeledEvent { label, .. } | ApprovalEvent::UnlabeledEvent { label } => {
                label.name.eq_ignore_ascii_case(required)
            }
            ApprovalEvent::RenamedTitleEvent { .. } => false,
        });
        let Some(ApprovalEvent::LabeledEvent { id, at, actor, .. }) = effective else {
            return GitHubAdmission::denied(format!(
                "no verifiable approval event for {required:?}; remove and reapply the label"
            ));
        };
        if last_edit.is_some_and(|edit| edit >= *at) {
            return GitHubAdmission::denied(format!(
                "title or body edited at/after approval for {required:?}; review changes, then remove and reapply the label"
            ));
        }
        let Some(actor) = actor.as_ref().filter(|a| !a.login.is_empty()) else {
            return GitHubAdmission::denied(format!("approval actor unavailable for {required:?}; reapproval required"));
        };
        if !cfg.trusted_approvers.is_empty() && !cfg.trusted_approvers.iter().any(|a| a.eq_ignore_ascii_case(&actor.login)) {
            return GitHubAdmission::denied(format!(
                "{required:?} applied by untrusted actor {}; a trusted approver must remove and reapply it",
                actor.login
            ));
        }
        approvals.push(format!("{required} by {} at {at} (event {id})", actor.login));
    }
    GitHubAdmission { approved: true, reason: format!("approved: {}", approvals.join("; ")) }
}
