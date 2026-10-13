//! Deterministic issue routing. Destinations are registry IDs, never issue-provided paths.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::domain::{Task, TaskSource};
use crate::priority::{Predicate, policy::predicates, rules::condition_matches};

/// Global routing rules in config.toml, independent of execution and priority settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    pub rules: Vec<RoutingRule>,
}

/// All predicates must match. Matching rules must agree on a single destination.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingRule {
    pub name: String,
    pub project: String,
    pub all: Vec<Predicate>,
}

/// Persisted evidence for the latest routing evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingDecision {
    /// Selected registry ID, absent for unmatched, conflicting or invalid routing.
    pub project: Option<String>,
    pub matched_rules: Vec<String>,
    pub reason: String,
    /// Whether the decision agrees with the task's saved assignment.
    pub permitted: bool,
    /// Assignment retained by an existing task; None for a raw evaluation.
    pub saved_project: Option<String>,
}

impl RoutingDecision {
    /// Human-readable selection and any conflict with the retained assignment.
    pub fn explanation(&self) -> String {
        if let (Some(selected), Some(saved)) = (&self.project, &self.saved_project)
            && selected != saved
        {
            return format!(
                "{}; task retains `{saved}`; restore routing or deliberately reassign unstarted work with task project",
                self.reason
            );
        }
        self.reason.clone()
    }

    /// A failed decision which cannot authorize a launch.
    pub fn blocked(reason: impl Into<String>) -> Self {
        Self { project: None, matched_rules: Vec::new(), reason: reason.into(), permitted: false, saved_project: None }
    }
}

impl Config {
    /// Validate routing predicates and every configured destination, including fallbacks.
    pub fn routing_problems(&self) -> Vec<String> {
        let projects = self.project_ids();
        let mut errors = Vec::new();
        let mut names = BTreeSet::new();
        for rule in &self.routing.rules {
            let path = format!("routing rule `{}`", rule.name);
            if rule.name.trim().is_empty() || !names.insert(&rule.name) {
                errors.push(format!("{path}: name must be nonempty and unique"));
            }
            if !projects.contains(&rule.project) {
                errors.push(format!("{path}: unknown execution project `{}`", rule.project));
            }
            if let Err(e) = predicates(&rule.all) {
                errors.push(format!("{path}: {e}"));
            }
            for p in &rule.all {
                if !matches!(p.field.as_str(), "source" | "repository" | "team" | "project" | "label") {
                    errors
                        .push(format!("{path}: routing field `{}` must be source, repository, team, project or label", p.field));
                }
            }
        }
        if self.linear.enabled
            && (!self.projects.is_empty() || !self.repo.path.is_empty())
            && !self.linear.execution_project.is_empty()
            && !projects.contains(&self.linear.execution_project)
        {
            errors.push(format!("Linear intake requires a registered default execution project: unknown `{}`; set linear.execution_project or use an empty fallback with routing rules", self.linear.execution_project));
        }
        errors
    }

    /// Evaluate rules before the source fallback. Conflicts and unknown IDs fail closed.
    /// Manual tasks retain their explicitly assigned project. Rule order has no precedence.
    pub fn route(&self, task: &Task) -> RoutingDecision {
        if matches!(task.source, TaskSource::Manual) {
            return self.route_destination(&task.execution_project, Vec::new(), "manual assignment");
        }
        if let Some(error) = self.routing_problems().first() {
            return RoutingDecision::blocked(error);
        }
        // Source identity must still be configured, even when a rule overrides its fallback.
        let fallback = match &task.source {
            TaskSource::GitHub { repository, .. } => match self.github_source(repository) {
                Ok(source) => source.execution_project.as_str(),
                Err(e) => return RoutingDecision::blocked(format!("{e:#}")),
            },
            TaskSource::Linear { .. } => self.linear.execution_project.as_str(),
            TaskSource::Manual => unreachable!("manual tasks handled above"),
        };
        let mut destinations = BTreeSet::new();
        let mut matched = Vec::new();
        for rule in &self.routing.rules {
            let Ok(conditions) = predicates(&rule.all) else {
                return RoutingDecision::blocked(format!("invalid routing rule `{}`", rule.name));
            };
            if conditions.iter().all(|p| condition_matches(p, task)) {
                destinations.insert(rule.project.as_str());
                matched.push(rule.name.clone());
            }
        }
        matched.sort();
        if destinations.len() > 1 {
            return RoutingDecision {
                project: None,
                reason: format!(
                    "conflicting routing rules [{}] select projects [{}]",
                    matched.join(", "),
                    destinations.into_iter().collect::<Vec<_>>().join(", ")
                ),
                matched_rules: matched,
                permitted: false,
                saved_project: None,
            };
        }
        if let Some(project) = destinations.first() {
            self.route_destination(project, matched, "matching routing rules override source fallback")
        } else {
            self.route_destination(fallback, matched, "no routing rules matched; source fallback")
        }
    }

    fn route_destination(&self, project: &str, matched_rules: Vec<String>, origin: &str) -> RoutingDecision {
        if project.is_empty() {
            return RoutingDecision::blocked("no routing rules matched and no source fallback is configured");
        }
        if !self.project_ids().contains(project) {
            return RoutingDecision::blocked(format!("unknown execution project `{project}` ({origin})"));
        }
        RoutingDecision {
            project: Some(project.into()),
            matched_rules,
            reason: format!("{origin}: `{project}`"),
            permitted: true,
            saved_project: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(r#"
            [repo]
            path = "/repos/default"
            [projects.api.repo]
            path = "/repos/api"
            [github]
            enabled = true
            repository = "acme/issues"
            [routing]
            [[routing.rules]]
            name = "api team and project"
            project = "api"
            all = [{field = "source", op = "eq", value = "linear"}, {field = "team", op = "eq", value = "API"}, {field = "project", op = "matches", value = "^Backend$"}]
            [[routing.rules]]
            name = "github override"
            project = "api"
            all = [{field = "repository", op = "eq", value = "acme/issues"}, {field = "label", op = "eq", value = "repo/api"}]
        "#).unwrap()
    }

    #[test]
    fn source_fallback_and_deliberate_override_share_policy_conditions() {
        let cfg = config();
        assert!(cfg.routing_problems().is_empty());
        let mut task =
            Task::new("GH-1", "ignored", TaskSource::GitHub { repository: "ACME/ISSUES".into(), number: 1, url: String::new() });
        assert_eq!(cfg.route(&task).project.as_deref(), Some("default"));
        task.labels.push("repo/api".into());
        assert_eq!(cfg.route(&task).project.as_deref(), Some("api"));
        task.source =
            TaskSource::Linear { issue_id: "id".into(), identifier: "API-1".into(), url: String::new(), team_key: "api".into() };
        assert_eq!(cfg.route(&task).project.as_deref(), Some("default"));
        task.project = Some("backend".into());
        assert_eq!(cfg.route(&task).project.as_deref(), Some("api"));
    }

    #[test]
    fn unmatched_unknown_and_invalid_conditions_fail_closed() {
        let mut cfg = config();
        cfg.github.execution_project.clear();
        let task = Task::new(
            "GH-1",
            "/tmp/attacker",
            TaskSource::GitHub { repository: "acme/issues".into(), number: 1, url: String::new() },
        );
        assert!(cfg.route(&task).reason.contains("no source fallback"));
        cfg.routing.rules[0].project = "/tmp/attacker".into();
        assert!(cfg.routing_problems()[0].contains("unknown execution project"));
        assert!(!cfg.route(&task).permitted);
        cfg.routing.rules[0].project = "api".into();
        cfg.routing.rules[0].all[0].field = "title".into();
        assert!(cfg.routing_problems().iter().any(|e| e.contains("routing field")));
        cfg.routing.rules[0].all.clear();
        assert!(cfg.routing_problems().iter().any(|e| e.contains("at least one condition")));
        assert!(toml::from_str::<RoutingConfig>("unknown = true").is_err());
        assert!(
            toml::from_str::<RoutingConfig>(
                "[[rules]]\nname='x'\nproject='api'\nall=[{field='label',op='eq',value='api',typo=true}]"
            )
            .is_err()
        );
    }

    #[test]
    fn agreeing_rules_are_not_conflicts() {
        let mut cfg = config();
        cfg.routing.rules.push(cfg.routing.rules[1].clone());
        cfg.routing.rules[2].name = "another agreeing rule".into();
        let mut task =
            Task::new("GH-1", "title", TaskSource::GitHub { repository: "acme/issues".into(), number: 1, url: String::new() });
        task.labels.push("repo/api".into());
        let route = cfg.route(&task);
        assert!(route.permitted);
        assert_eq!(route.matched_rules.len(), 2);
    }
}

#[cfg(test)]
mod properties {
    use crate::strategies::routing_case;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn order_independent_and_conflicts_never_pick_a_project((mut cfg, task) in routing_case()) {
            let decision = cfg.route(&task);
            cfg.routing.rules.reverse();
            prop_assert_eq!(&decision, &cfg.route(&task));
            if task.labels.len() == 2 {
                prop_assert!(!decision.permitted);
                prop_assert!(decision.project.is_none());
                prop_assert_eq!(decision.matched_rules.len(), 2);
            } else {
                prop_assert!(decision.permitted);
                prop_assert!(cfg.project_ids().contains(decision.project.as_ref().unwrap()));
            }
        }
    }
}
