//! Execution project configuration, independent of issue sources and admission.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{AppliedOverrides, CleanupConfig, Config, PriorityConfig, PromptConfig, RepoConfig};
use crate::domain::Task;
use crate::paths::Paths;
use crate::priority::PriorityRules;
use crate::worktree::Repo;

/// Repository-specific settings. Budgets, source admission and concurrency
/// belong to the machine configuration and are deliberately absent here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectConfig {
    pub repo: RepoConfig,
    pub prompt: Option<PromptConfig>,
    pub cleanup: Option<CleanupConfig>,
    pub priority: Option<PriorityConfig>,
}

impl Config {
    /// Registered IDs, including the legacy `repo` under `default`.
    pub fn project_ids(&self) -> BTreeSet<String> {
        let mut ids: BTreeSet<_> = self.projects.keys().cloned().collect();
        if !self.repo.path.trim().is_empty() {
            ids.insert("default".into());
        }
        ids
    }

    /// Resolve one project's effective configuration. Unknown IDs, invalid
    /// settings and unreadable overrides fail; never fall back to another repo.
    /// Machine budgets, source settings and total concurrency remain global.
    pub fn for_project(&self, id: &str, paths: &Paths) -> Result<Self> {
        if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
            bail!("invalid execution project ID `{id}`");
        }
        if self.projects.is_empty() && id == "default" {
            return Ok(self.clone());
        }
        let mut cfg = self.clone();
        cfg.projects.clear();
        cfg.overrides = AppliedOverrides::default();
        if let Some(project) = self.projects.get(id) {
            cfg.repo = project.repo.clone();
            if let Some(prompt) = &project.prompt {
                cfg.prompt = prompt.clone();
                cfg.prompt.base_dir = self.prompt.base_dir.clone();
            }
            if let Some(cleanup) = &project.cleanup {
                cfg.cleanup = cleanup.clone();
            }
            if let Some(priority) = &project.priority {
                cfg.priority = priority.clone();
            }
            let root = project
                .repo
                .worktree_root
                .as_ref()
                .map(|p| crate::paths::expand_tilde(p))
                .unwrap_or_else(|| paths.data_dir.join("worktrees/projects"));
            cfg.repo.worktree_root = Some(root.join(id).to_string_lossy().into_owned());
        } else if id != "default" || self.repo.path.trim().is_empty() {
            bail!("unknown execution project `{id}`; register it in config.toml");
        }
        let repo = cfg.repo_path();
        cfg.apply_repo_overrides(&repo).with_context(|| format!("execution project `{id}`"))?;
        cfg.budget = self.budget.clone();
        cfg.linear = self.linear.clone();
        cfg.github = self.github.clone();
        cfg.scheduler.max_concurrent = self.scheduler.max_concurrent;
        cfg.scheduler.tick_secs = self.scheduler.tick_secs;
        cfg.scheduler.resource_sample_secs = self.scheduler.resource_sample_secs;
        cfg.overrides.keys.retain(|k| {
            !k.starts_with("linear.")
                && !k.starts_with("github.")
                && !["scheduler.max_concurrent", "scheduler.tick_secs", "scheduler.resource_sample_secs"].contains(&k.as_str())
        });
        // The resolved config has one checkout and no registry. Validate source
        // mappings against the machine registry, not this temporary projection.
        let mut routing = self.github_routing_problems();
        routing.extend(self.routing_problems());
        if !routing.is_empty() {
            bail!("{}", routing.join("; "));
        }
        let github = std::mem::take(&mut cfg.github);
        let github_sources = std::mem::take(&mut cfg.github_sources);
        let routing = std::mem::take(&mut cfg.routing);
        let linear_project = std::mem::take(&mut cfg.linear.execution_project);
        cfg.ensure_valid().with_context(|| format!("execution project `{id}`"))?;
        cfg.routing = routing;
        cfg.linear.execution_project = linear_project;
        cfg.github = github;
        cfg.github_sources = github_sources;
        Ok(cfg)
    }

    /// Resolve a task and verify its pinned checkout and existing worktrees.
    /// Fails before any runtime operation if configuration would redirect it.
    pub fn for_task(&self, task: &Task, paths: &Paths) -> Result<Self> {
        let cfg = self.for_project(&task.execution_project, paths)?;
        if let Some(bound) = &task.execution_repo {
            let actual = cfg.repo_path().canonicalize().with_context(|| {
                format!("execution project `{}` checkout {}", task.execution_project, cfg.repo_path().display())
            })?;
            if actual != Path::new(bound) {
                bail!(
                    "task {} is pinned to project `{}` at {}; configured checkout is {}; restore the project path",
                    task.key,
                    task.execution_project,
                    bound,
                    actual.display()
                );
            }
        }
        let repo = Repo::new(cfg.repo_path());
        for path in task.worktree_path.iter().chain(task.review.as_ref().and_then(|r| r.worktree_path.as_ref())) {
            repo.ensure_worktree_owner(Path::new(path))?;
        }
        Ok(cfg)
    }

    /// Structural registry errors without filesystem access.
    pub fn project_problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.projects.contains_key("default") && !self.repo.path.trim().is_empty() {
            out.push("configure either repo or projects.default.repo, not both".into());
        }
        for (id, project) in &self.projects {
            if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
                out.push(format!("project ID `{id}` must contain only ASCII letters, digits, - or _"));
            }
            if project.repo.path.trim().is_empty() {
                out.push(format!("projects.{id}.repo.path is empty"));
            } else if !crate::paths::expand_tilde(&project.repo.path).is_absolute() {
                out.push(format!("projects.{id}.repo.path must be absolute"));
            }
            if !project.repo.branch_template.contains("{key}") && !project.repo.branch_template.contains("{id}") {
                out.push(format!("projects.{id}.repo.branch_template must contain {{key}} or {{id}}"));
            }
        }
        out
    }
}

/// Read the resolved project's rules. Missing files use defaults; invalid or
/// unreadable files return an error so this project cannot silently bypass policy.
pub fn load_rules(cfg: &Config, paths: &Paths) -> Result<PriorityRules> {
    let source = cfg.rules_source(paths);
    match source.read().with_context(|| format!("read {}", source.describe()))? {
        None => Ok(PriorityRules::default()),
        Some(text) => PriorityRules::parse(&text).map_err(|errors| {
            anyhow::anyhow!("{}: {}", source.describe(), errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskSource;

    #[test]
    fn registry_boundaries_and_legacy_default() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(tmp.path());
        let mut cfg = Config::default();
        cfg.repo.path = tmp.path().display().to_string();
        assert_eq!(cfg.for_project("default", &paths).unwrap(), cfg);
        assert!(cfg.for_project("missing", &paths).is_err());
        let mut project = ProjectConfig::default();
        project.repo.path = tmp.path().display().to_string();
        project.repo.setup = vec!["echo project".into()];
        cfg.projects.insert("api".into(), project);
        cfg.scheduler.max_concurrent = 2;
        std::fs::write(tmp.path().join(".powerqueue.toml"), "instructions = 'local prompt'\n[scheduler]\nmax_concurrent = 100\nmax_attempts = 9\n[linear]\nenabled = false\n[cleanup]\nremove_worktree = false\n").unwrap();
        let resolved = cfg.for_project("api", &paths).unwrap();
        assert_eq!(resolved.scheduler.max_concurrent, 2);
        assert_eq!(resolved.scheduler.max_attempts, 9);
        assert_eq!(resolved.budget, cfg.budget);
        assert_eq!(resolved.linear, cfg.linear);
        assert!(!resolved.cleanup.remove_worktree);
        assert_eq!(resolved.repo.setup, ["echo project"]);
        assert!(resolved.claude.append_system_prompt.as_ref().unwrap().contains("local prompt"));
        assert_ne!(resolved.worktree_root(&paths), cfg.for_project("default", &paths).unwrap().worktree_root(&paths));
        let mut t = Task::new("T", "test", TaskSource::Manual);
        t.project = Some("Linear source project".into());
        t.execution_project = "api".into();
        t.execution_repo = Some("/different/checkout".into());
        assert!(cfg.for_task(&t, &paths).unwrap_err().to_string().contains("pinned"));
    }

    #[test]
    fn registry_rejects_unsafe_ids_and_machine_settings() {
        let cfg = Config::from_toml("[projects.'../escape'.repo]\npath = '/tmp/repo'\n").unwrap();
        assert!(cfg.validate().iter().any(|p| p.contains("project ID")));
        assert!(Config::from_toml("[projects.api.budget]\nsafety_margin = 0.0\n").is_err());
        assert!(Config::from_toml("[projects.api.scheduler]\nmax_concurrent = 10\n").is_err());
        let cfg = Config::from_toml("[repo]\npath = '/one'\n[projects.default.repo]\npath = '/two'\n").unwrap();
        assert!(cfg.validate().iter().any(|p| p.contains("not both")));
    }
}
