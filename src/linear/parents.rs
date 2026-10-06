//! Parent issues (containers): which ones to watch and when to close them.
//!
//! An issue with sub-issues is never run. The daemon keeps the identifiers
//! of parents it has seen in the kv store ([`WATCHED_PARENTS_KEY`]), looks
//! them up on every Linear poll and, once [`container_finished`] holds,
//! moves the parent to `linear.done_state_parent` and posts
//! [`container_comment`]. The parent does not have to be a powerqueue task:
//! it is usually `In Progress` while its children are worked on.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::domain::LinkedIssue;

use super::LinearIssue;

/// kv key holding [`WatchedParents`].
pub const WATCHED_PARENTS_KEY: &str = "linear.watched_parents";

/// How many closed parents are remembered (oldest dropped first).
const CLOSED_MEMORY: usize = 500;

/// Parent issues the daemon is watching, and the ones it already closed
/// (so a parent left open — `linear.manage_states = false`, no
/// `done_state_parent` — is not commented on again every poll).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedParents {
    #[serde(default)]
    pub watching: BTreeSet<String>,
    /// Closed by powerqueue, most recent last.
    #[serde(default)]
    pub closed: Vec<String>,
}

impl WatchedParents {
    /// Start watching `keys`, except the ones already closed by powerqueue.
    pub fn watch(&mut self, keys: impl IntoIterator<Item = String>) {
        for key in keys {
            if !self.closed.contains(&key) {
                self.watching.insert(key);
            }
        }
    }

    /// Stop watching `key` and remember that powerqueue closed it.
    pub fn mark_closed(&mut self, key: &str) {
        self.watching.remove(key);
        self.closed.retain(|k| k != key);
        self.closed.push(key.to_string());
        let excess = self.closed.len().saturating_sub(CLOSED_MEMORY);
        self.closed.drain(..excess);
    }
}

/// Identifiers worth watching from a batch of fetched issues: every parent
/// of a sub-issue, and every issue that has sub-issues itself.
pub fn parents_to_watch(issues: &[LinearIssue]) -> BTreeSet<String> {
    issues
        .iter()
        .filter_map(|i| i.parent.clone())
        .chain(issues.iter().filter(|i| !i.children.is_empty()).map(|i| i.identifier.clone()))
        .collect()
}

/// True once a parent can be closed: it has sub-issues, all of them are
/// closed (completed or canceled) and at least one was completed. A parent
/// whose children were all canceled is left for a human to decide.
pub fn container_finished(children: &[LinkedIssue]) -> bool {
    !children.is_empty()
        && children.iter().all(LinkedIssue::is_closed)
        && children.iter().any(|c| c.state_type.trim().eq_ignore_ascii_case("completed"))
}

/// Comment (in Spanish, like the team's tickets) posted on a parent when it
/// is closed: why, and the list of sub-issues with their outcome. `state` is
/// the workflow state the parent is moved to, if any.
pub fn container_comment(children: &[LinkedIssue], state: Option<&str>) -> String {
    let mut out = match state {
        Some(state) => format!("Todas las sub-issues están cerradas; powerqueue mueve este ticket a **{state}**.\n"),
        None => "Todas las sub-issues están cerradas.\n".to_string(),
    };
    out.push('\n');
    for c in children {
        let outcome = if c.state_type.trim().eq_ignore_ascii_case("completed") { "completada" } else { "cancelada" };
        if c.title.trim().is_empty() {
            out.push_str(&format!("- {} ({outcome})\n", c.key));
        } else {
            out.push_str(&format!("- {} — {} ({outcome})\n", c.key, c.title.trim()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linked(key: &str, title: &str, state_type: &str) -> LinkedIssue {
        LinkedIssue { key: key.into(), title: title.into(), state_type: state_type.into(), pr_merged: false }
    }

    #[test]
    fn finished_needs_every_child_closed_and_one_completed() {
        assert!(!container_finished(&[]));
        assert!(!container_finished(&[linked("A-1", "", "completed"), linked("A-2", "", "started")]));
        assert!(container_finished(&[linked("A-1", "", "completed"), linked("A-2", "", "canceled")]));
        assert!(!container_finished(&[linked("A-1", "", "canceled")]), "all canceled is a human call");
        let mut merged = linked("A-3", "", "started");
        merged.pr_merged = true;
        assert!(!container_finished(&[linked("A-1", "", "completed"), merged]), "a merged PR is not a closed child");
    }

    #[test]
    fn comment_lists_children_in_spanish() {
        let children = [linked("A-1", "Primero", "completed"), linked("A-2", "", "canceled")];
        let c = container_comment(&children, Some("Done"));
        assert_eq!(
            c,
            "Todas las sub-issues están cerradas; powerqueue mueve este ticket a **Done**.\n\n- A-1 — Primero (completada)\n- A-2 (cancelada)\n"
        );
        assert!(container_comment(&children, None).starts_with("Todas las sub-issues están cerradas.\n\n- A-1"));
    }

    #[test]
    fn closed_parents_are_not_watched_again() {
        let mut w = WatchedParents::default();
        w.watch(["P-1".to_string(), "P-2".to_string()]);
        w.mark_closed("P-1");
        w.watch(["P-1".to_string()]);
        assert_eq!(w.watching.iter().collect::<Vec<_>>(), vec!["P-2"]);
        for i in 0..CLOSED_MEMORY + 3 {
            w.mark_closed(&format!("X-{i}"));
        }
        assert_eq!(w.closed.len(), CLOSED_MEMORY);
        assert_eq!(w.closed.last().map(String::as_str), Some(format!("X-{}", CLOSED_MEMORY + 2).as_str()));
    }

    #[test]
    fn watch_parents_and_containers() {
        let issue = LinearIssue {
            parent: Some("AVS-1429".into()),
            children: vec![linked("AVS-1720", "", "completed")],
            ..plain("AVS-1714")
        };
        let watched = parents_to_watch(&[issue, plain("AVS-1")]);
        assert_eq!(watched.into_iter().collect::<Vec<_>>(), vec!["AVS-1429".to_string(), "AVS-1714".to_string()]);
    }

    fn plain(key: &str) -> LinearIssue {
        LinearIssue {
            id: key.into(),
            identifier: key.into(),
            title: String::new(),
            description: String::new(),
            url: String::new(),
            priority: 0,
            estimate: None,
            labels: Vec::new(),
            state_name: "Todo".into(),
            state_type: "unstarted".into(),
            team_key: "AVS".into(),
            project: None,
            assignee_id: None,
            cycle: None,
            blocked_by: Vec::new(),
            children: Vec::new(),
            parent: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }
}
