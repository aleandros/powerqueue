//! Short task-key labels for the task table (display only).
//!
//! GitHub keys (`owner/repo#83`) share a long prefix, so a narrow column cut
//! from the right shows the same owner on every row. The table shows
//! `repo#83` instead, adds the owner only when two owners have a repository
//! with the same name, and truncates from the left so the number stays
//! visible. Stored keys, the detail pane and CLI output keep the full key.

use std::collections::{BTreeMap, BTreeSet};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::domain::{Task, TaskSource};

/// Labels for one set of tasks; repository names are compared case-insensitively.
#[derive(Debug, Clone, Default)]
pub struct KeyLabels {
    /// Lower-cased repository names that exist under more than one owner.
    ambiguous: BTreeSet<String>,
}

impl KeyLabels {
    /// Look at every GitHub task in `tasks` (visible or not, so labels do not
    /// change when a filter hides a row) to find repository names that need
    /// their owner.
    pub fn new(tasks: &[Task]) -> Self {
        let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for task in tasks {
            if let TaskSource::GitHub { repository, .. } = &task.source {
                let (owner, repo) = split_repository(repository);
                owners.entry(repo.to_lowercase()).or_default().insert(owner.to_lowercase());
            }
        }
        let ambiguous = owners.into_iter().filter(|(_, o)| o.len() > 1).map(|(repo, _)| repo).collect();
        Self { ambiguous }
    }

    /// Table label for `task`: `repo#N` or `owner/repo#N` for GitHub issues,
    /// the stored key for everything else.
    pub fn label(&self, task: &Task) -> String {
        match &task.source {
            TaskSource::GitHub { repository, number, .. } => {
                let (owner, repo) = split_repository(repository);
                if owner.is_empty() || !self.ambiguous.contains(&repo.to_lowercase()) {
                    format!("{repo}#{number}")
                } else {
                    format!("{owner}/{repo}#{number}")
                }
            }
            TaskSource::Linear { .. } | TaskSource::Manual => task.key.clone(),
        }
    }
}

/// The repository name of a GitHub task, if `task` is one.
pub fn github_repo(task: &Task) -> Option<&str> {
    match &task.source {
        TaskSource::GitHub { repository, .. } => Some(split_repository(repository).1),
        TaskSource::Linear { .. } | TaskSource::Manual => None,
    }
}

fn split_repository(repository: &str) -> (&str, &str) {
    repository.split_once('/').unwrap_or(("", repository))
}

/// Fit `label` into `max` columns. A trailing `#N` is never cut: the part
/// before it loses its start (`…queue#83`), or its middle when it names an
/// owner (`al…/app#1`), and gets `ellipsis` in place of what was removed.
/// Labels without `#N` are cut from the right. When even `#N` does not fit,
/// it is returned whole and the column clips it.
pub fn fit_key(label: &str, max: usize, ellipsis: &str) -> String {
    if label.width() <= max {
        return label.to_string();
    }
    let Some((prefix, number)) = label.rsplit_once('#').filter(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    else {
        return truncate_right(label, max, ellipsis);
    };
    let suffix = format!("#{number}");
    let budget = max.saturating_sub(suffix.width() + ellipsis.width());
    if budget == 0 {
        return if max >= suffix.width() + ellipsis.width() { format!("{ellipsis}{suffix}") } else { suffix };
    }
    let head = if prefix.contains('/') { budget / 2 } else { 0 };
    let start = take_width(prefix.chars(), head);
    let end: String = take_width(prefix.chars().rev(), budget - start.width()).chars().rev().collect();
    format!("{start}{ellipsis}{end}{suffix}")
}

fn truncate_right(s: &str, max: usize, ellipsis: &str) -> String {
    let mut out = take_width(s.chars(), max.saturating_sub(ellipsis.width()));
    out.push_str(ellipsis);
    out
}

/// The longest run of `chars` (in order) that fits in `width` columns.
fn take_width(chars: impl Iterator<Item = char>, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in chars {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github(repository: &str, number: u64) -> Task {
        let source = TaskSource::GitHub { repository: repository.into(), number, url: String::new() };
        Task::new(format!("{repository}#{number}"), "t", source)
    }

    #[test]
    fn owner_is_dropped_unless_two_owners_share_a_repo_name() {
        let tasks = vec![
            github("aleandros/powerqueue", 83),
            github("aleandros/acerosvs", 83),
            github("alice/app", 1),
            github("Bob/App", 2),
        ];
        let labels = KeyLabels::new(&tasks);
        let got: Vec<_> = tasks.iter().map(|t| labels.label(t)).collect();
        assert_eq!(got, ["powerqueue#83", "acerosvs#83", "alice/app#1", "Bob/App#2"]);
        let manual = Task::new("ENG-7", "t", TaskSource::Manual);
        assert_eq!(labels.label(&manual), "ENG-7");
        assert_eq!(github_repo(&tasks[0]), Some("powerqueue"));
        assert_eq!(github_repo(&manual), None);
    }

    #[test]
    fn fitting_keeps_the_issue_number() {
        assert_eq!(fit_key("powerqueue#83", 13, "…"), "powerqueue#83");
        assert_eq!(fit_key("powerqueue#83", 9, "…"), "…queue#83");
        assert_eq!(fit_key("powerqueue#83", 9, "..."), "...eue#83");
        assert_eq!(fit_key("alice/app#1", 8, "…"), "al…app#1");
        assert_eq!(fit_key("powerqueue#83", 4, "…"), "…#83");
        assert_eq!(fit_key("powerqueue#12345", 4, "…"), "#12345");
        assert_eq!(fit_key("LONG-KEY-123", 6, "…"), "LONG-…");
        assert_eq!(fit_key("note#draft", 6, "…"), "note#…");
    }
}

#[cfg(test)]
mod properties {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn fitted_github_labels_fit_and_keep_the_number(
            owner in "[a-z]{0,12}",
            repo in "[a-z-]{1,24}",
            number in 1u64..100_000,
            max in 0usize..30,
            ascii in any::<bool>(),
        ) {
            let ellipsis = if ascii { "..." } else { "…" };
            let label = if owner.is_empty() { format!("{repo}#{number}") } else { format!("{owner}/{repo}#{number}") };
            let fitted = fit_key(&label, max, ellipsis);
            let suffix = format!("#{number}");
            prop_assert!(fitted.ends_with(&suffix), "{fitted:?} lost {suffix}");
            prop_assert!(fitted.width() <= max.max(suffix.width()), "{fitted:?} wider than {max}");
            if label.width() <= max {
                prop_assert_eq!(fitted, label);
            }
        }
    }
}
