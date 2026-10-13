//! Source configuration lookup and credentials. Routing never falls back to a checkout.

use anyhow::{Context, Result, bail};

use crate::config::{Config, GitHubConfig};
use crate::secrets::{SecretKind, Secrets};

impl Config {
    /// Configured sources, including the legacy source when it has a repository.
    /// Disabled sources are included so explicit CLI previews can inspect them.
    pub fn github_sources(&self) -> Vec<&GitHubConfig> {
        std::iter::once(&self.github)
            .filter(|s| s.enabled || !s.repository.is_empty())
            .chain(self.github_sources.iter())
            .collect()
    }

    /// Resolve a unique enabled source by repository (case insensitive).
    /// Unknown, disabled or conflicting sources fail closed.
    pub fn github_source(&self, repository: &str) -> Result<&GitHubConfig> {
        let matches: Vec<_> =
            self.github_sources().into_iter().filter(|s| s.repository.eq_ignore_ascii_case(repository)).collect();
        let [source] = matches.as_slice() else {
            bail!("GitHub source `{repository}` is missing or has conflicting assignments");
        };
        if !source.enabled {
            bail!("GitHub source `{repository}` is disabled");
        }
        if !source.execution_project.is_empty() && !self.project_ids().contains(&source.execution_project) {
            bail!("GitHub source `{repository}` maps to unknown execution project `{}`", source.execution_project);
        }
        Ok(source)
    }

    /// Structural mapping errors, including duplicate repositories across API hosts.
    /// Source keys use owner/repo, so one such identity may be configured only once.
    pub fn github_routing_problems(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let projects = self.project_ids();
        let mut problems = Vec::new();
        for source in self.github_sources() {
            if !seen.insert(source.repository.to_ascii_lowercase()) {
                problems.push(format!(
                    "GitHub source `{}` has conflicting assignments; configure each repository only once",
                    source.repository
                ));
            }
            if !source.execution_project.is_empty() && !projects.contains(&source.execution_project) {
                problems.push(format!(
                    "GitHub source `{}` maps to unknown execution project `{}`",
                    source.repository, source.execution_project
                ));
            }
        }
        problems
    }
}

impl GitHubConfig {
    /// Resolve this source's token and its origin without exposing the token in errors.
    /// An explicit missing/empty environment variable fails without global fallback.
    pub fn token(&self, secrets: &Secrets) -> Result<(String, String)> {
        if let Some(name) = &self.token_env {
            let token = std::env::var(name)
                .map_err(|_| anyhow::anyhow!("GitHub token environment variable {name} is missing or not valid Unicode"))?;
            if token.trim().is_empty() {
                bail!("GitHub token environment variable {name} is empty");
            }
            return Ok((token, format!("env {name}")));
        }
        let (token, origin) = secrets
            .get_with_origin(SecretKind::GitHubToken)?
            .context("no GitHub token configured; run `powerqueue secrets set github` or set GITHUB_TOKEN")?;
        Ok((token, origin.to_string()))
    }
}
