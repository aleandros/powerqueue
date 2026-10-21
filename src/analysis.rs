//! Optional issue analysis and input-bound repository selection; source approval stays separate.

pub mod client;

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::domain::{Task, TaskId};

/// Explicit opt-in; trusted descriptions are separate from executable routing rules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalysisConfig {
    pub enabled: bool,
    pub jev_first: bool,
    pub min_confidence: f64,
    pub timeout_secs: u64,
    pub projects: BTreeMap<String, String>,
    pub llm: Option<TextModelConfig>,
    pub claude_code: Option<ClaudeCodeConfig>,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            jev_first: true,
            min_confidence: 0.8,
            timeout_secs: 20,
            projects: BTreeMap::new(),
            llm: None,
            claude_code: None,
        }
    }
}

/// Bounded, tool-free Claude Code fallback using the server's existing login.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeCodeConfig {
    pub binary: String,
    pub model: String,
}
impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        Self { binary: "claude".into(), model: "sonnet".into() }
    }
}

/// A tool-free Anthropic Messages-compatible endpoint; uses a separate API key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TextModelConfig {
    pub endpoint: String,
    pub model: String,
    pub max_tokens: u32,
}

impl Default for TextModelConfig {
    fn default() -> Self {
        Self { endpoint: "https://api.anthropic.com/v1/messages".into(), model: String::new(), max_tokens: 1024 }
    }
}

impl Config {
    /// Validate analysis settings without contacting models or opening repositories.
    pub fn analysis_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let cfg = &self.analysis;
        if !cfg.min_confidence.is_finite() || !(0.5..=1.0).contains(&cfg.min_confidence) {
            problems.push("analysis.min_confidence must be finite and between 0.5 and 1".into());
        }
        if !(1..=60).contains(&cfg.timeout_secs) {
            problems.push("analysis.timeout_secs must be between 1 and 60".into());
        }
        if cfg.projects.len() > 32 || (cfg.enabled && cfg.projects.is_empty()) {
            problems.push("enabled analysis needs 1–32 described projects".into());
        }
        for (id, description) in &cfg.projects {
            if !self.project_ids().contains(id) {
                problems.push(format!("analysis.projects.{id} is not a registered execution project"));
            }
            if description.trim().is_empty() || description.len() > 2000 {
                problems.push(format!("analysis.projects.{id} needs a description of 1–2000 bytes"));
            }
        }
        if cfg.enabled && !cfg.jev_first && cfg.llm.is_none() && cfg.claude_code.is_none() {
            problems.push("analysis needs jev_first, analysis.claude_code or an analysis.llm endpoint".into());
        }
        if cfg.enabled
            && cfg.jev_first
            && (!valid_endpoint(&self.priority.jev.endpoint) || self.priority.jev.model.trim().is_empty())
        {
            problems.push("analysis requires a valid priority.jev endpoint and nonempty model".into());
        }
        if let Some(claude) = &cfg.claude_code {
            if cfg.llm.is_some() {
                problems.push("choose only one fallback: analysis.claude_code or analysis.llm".into());
            }
            if claude.model.trim().is_empty()
                || crate::session::BinaryTemplate::parse(&claude.binary).map_or(true, |command| command.is_per_task())
            {
                problems.push("analysis.claude_code needs a nonempty model and host binary without task placeholders".into());
            }
        }
        if let Some(llm) = &cfg.llm {
            if !valid_endpoint(&llm.endpoint) {
                problems.push(
                    "analysis.llm.endpoint must use HTTPS (HTTP only on loopback), without credentials, query or fragment".into(),
                );
            }
            if llm.model.trim().is_empty() || !(128..=4096).contains(&llm.max_tokens) {
                problems.push("analysis.llm needs a nonempty model and max_tokens between 128 and 4096".into());
            }
        }
        problems
    }
}

fn valid_endpoint(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint).is_ok_and(|url| {
        let local = url.host_str().is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "[::1]"));
        (url.scheme() == "https" || (url.scheme() == "http" && local))
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

/// Snapshot sent to a model. Contains no repository paths, credentials or tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisInput {
    pub issue: serde_json::Value,
    pub projects: BTreeMap<String, String>,
}

impl AnalysisInput {
    /// Build bounded input; oversized content is rejected rather than silently omitted.
    pub fn new(cfg: &Config, task: &Task) -> Result<Self> {
        let problems = cfg.analysis_problems();
        if !problems.is_empty() {
            bail!("invalid analysis configuration: {}", problems.join("; "));
        }
        let input = Self {
            issue: serde_json::json!({
                "title": task.title, "description": task.description, "labels": task.labels,
                "source": task.source, "source_project": task.project,
            }),
            projects: cfg.analysis.projects.clone(),
        };
        if serde_json::to_vec(&input)?.len() > 64 * 1024 {
            bail!("analysis input exceeds 64 KiB; shorten the issue or project descriptions before retrying");
        }
        Ok(input)
    }

    /// Stable identity includes inputs, model policy and registered checkout bindings.
    /// Changing descriptions, source content or registry settings invalidates old advice.
    pub fn fingerprint(&self, cfg: &Config) -> String {
        let value = serde_json::json!({
            "input": self, "analysis": cfg.analysis, "jev": cfg.priority.jev,
            "registry": cfg.projects, "default_repo": cfg.repo.path, "routing": cfg.routing,
        });
        format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
    }
}

/// Strict proposal schema; an LLM cannot add executable commands or approval fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub project: Option<String>,
    pub classification: Classification,
    pub confidence: f64,
    pub reasoning: String,
    pub plan: Vec<String>,
}

/// Descriptive category only; it never contributes to scheduler priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Bug,
    Feature,
    Question,
    Maintenance,
    Unknown,
}

impl Proposal {
    /// Reject unsupported destinations and malformed text, then withhold low-confidence advice.
    /// No task, routing rule, label, session, budget or approval is changed.
    pub fn validate(mut self, input: &AnalysisInput, threshold: f64) -> Result<Self> {
        if !threshold.is_finite()
            || !(0.5..=1.0).contains(&threshold)
            || !self.confidence.is_finite()
            || !(0.0..=1.0).contains(&self.confidence)
        {
            bail!("invalid analysis confidence");
        }
        if self.project.as_ref().is_some_and(|id| !input.projects.contains_key(id)) {
            bail!("model recommended an unsupported project; human resolution required");
        }
        if self.reasoning.trim().is_empty()
            || self.reasoning.len() > 8000
            || self.plan.len() > 10
            || self.plan.iter().any(|step| step.trim().is_empty() || step.len() > 2000)
        {
            bail!("model returned invalid reasoning or plan");
        }
        if self.project.is_some() && self.plan.is_empty() {
            bail!("model returned a project without a proposed plan");
        }
        if self.confidence < threshold {
            self.project = None;
            self.reasoning = format!("Confidence below {threshold:.2}; resolve with a human. {}", self.reasoning);
        }
        Ok(self)
    }
}

/// One bounded model request, including unsuccessful attempts in the fallback chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisAttempt {
    pub backend: String,
    pub model: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub error: Option<String>,
}

/// Durable advisory result, kept separately so a slow model cannot overwrite task changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisRecord {
    pub created_at: DateTime<Utc>,
    pub fingerprint: String,
    pub proposal: Option<Proposal>,
    pub attempts: Vec<AnalysisAttempt>,
}

impl AnalysisRecord {
    /// Advice is stale when its original input or trusted configuration no longer matches.
    pub fn stale(&self, cfg: &Config, task: &Task) -> bool {
        AnalysisInput::new(cfg, task).map_or(true, |input| input.fingerprint(cfg) != self.fingerprint)
    }

    /// Human-facing diagnostic, including unresolved and superseded model output.
    pub fn status(&self, cfg: &Config, task: &Task) -> &'static str {
        if self.stale(cfg, task) {
            "stale — rerun analysis"
        } else if self.proposal.as_ref().and_then(|p| p.project.as_ref()).is_none() {
            "unresolved — human resolution required"
        } else {
            if matches!(&task.project_assignment, Some(crate::routing::ProjectAssignment::Automatic { fingerprint, .. }) if *fingerprint == self.fingerprint)
            {
                "repository assigned — source approval still applies"
            } else {
                "recommendation — source approval still applies"
            }
        }
    }
}

/// Per-task analysis metadata key; assignment validates this record transactionally.
pub fn record_key(id: TaskId) -> String {
    format!("analysis.{id}")
}

#[cfg(test)]
mod properties {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn proposals_never_escape_registry_or_confidence_gate(proposal in crate::strategies::analysis_proposal()) {
            let input = AnalysisInput { issue: serde_json::json!({}), projects: [("default".into(), "API".into()), ("web".into(), "UI".into())].into() };
            let confidence = proposal.confidence;
            if let Ok(valid) = proposal.validate(&input, 0.8) {
                prop_assert!(valid.confidence.is_finite());
                if let Some(project) = valid.project {
                    prop_assert!(input.projects.contains_key(&project));
                    prop_assert!(confidence >= 0.8);
                    prop_assert!(!valid.plan.is_empty());
                }
            }
        }

        #[test]
        fn snapshots_track_material_input_not_task_authority((mut task, _) in crate::strategies::task_with_session(), edit in ".{1,100}") {
            let mut cfg = Config::default();
            cfg.repo.path = "/repo".into();
            cfg.analysis.projects.insert("default".into(), "API".into());
            let input = AnalysisInput::new(&cfg, &task).unwrap();
            let record = AnalysisRecord { created_at: crate::strategies::origin(), fingerprint: input.fingerprint(&cfg), proposal: None, attempts: vec![] };
            task.state = crate::domain::TaskState::Running;
            task.attempts = 99;
            task.execution_project = "web".into();
            prop_assert!(!record.stale(&cfg, &task), "lifecycle changes do not rewrite advisory input");
            task.description.push_str(&edit);
            prop_assert!(record.stale(&cfg, &task));
        }
    }

    #[test]
    fn legacy_repo_overrides_do_not_invalidate_machine_analysis() {
        let mut cfg = Config::default();
        cfg.repo.path = "/repo".into();
        cfg.analysis.projects.insert("default".into(), "Application".into());
        let task = Task::new("A", "title", crate::domain::TaskSource::Manual);
        let input = AnalysisInput::new(&cfg, &task).unwrap();
        let fingerprint = input.fingerprint(&cfg);
        cfg.repo.setup.push("npm ci".into());
        cfg.repo.default_branch = Some("main".into());
        assert_eq!(input.fingerprint(&cfg), fingerprint, "CLI and daemon must agree despite repo-local setup overrides");
        cfg.repo.path = "/other/repo".into();
        assert_ne!(input.fingerprint(&cfg), fingerprint);
    }

    #[test]
    fn configuration_and_input_are_bounded_and_opt_in() {
        let mut cfg = Config::default();
        assert!(!cfg.analysis.enabled);
        assert!(cfg.analysis_problems().is_empty());
        cfg.analysis.enabled = true;
        assert!(!cfg.analysis_problems().is_empty());
        cfg.repo.path = "/repo".into();
        cfg.analysis.projects.insert("default".into(), "API".into());
        assert!(cfg.analysis_problems().is_empty());
        for endpoint in ["http://remote.example/v1", "https://key@host/v1", "https://host/v1?key=secret", "file:///tmp/model"] {
            cfg.priority.jev.endpoint = endpoint.into();
            assert!(!cfg.analysis_problems().is_empty(), "{endpoint}");
        }
        cfg.priority.jev.endpoint = "https://api.typesafe.ai/v1/systemone".into();
        let mut task = Task::new("A", "title", crate::domain::TaskSource::Manual);
        task.description = "x".repeat(64 * 1024);
        assert!(AnalysisInput::new(&cfg, &task).is_err());
        assert!(toml::from_str::<AnalysisConfig>("enable = true").is_err());
        assert!(toml::from_str::<TextModelConfig>("tools = ['bash']").is_err());
    }

    #[test]
    fn machine_analysis_does_not_break_narrowed_execution_configs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(dir.path());
        let mut cfg = Config::default();
        cfg.linear.enabled = false;
        cfg.repo.path = dir.path().display().to_string();
        let mut web = crate::projects::ProjectConfig::default();
        web.repo.path = dir.path().join("web").display().to_string();
        std::fs::create_dir_all(&web.repo.path).unwrap();
        cfg.projects.insert("web".into(), web);
        cfg.analysis.enabled = true;
        cfg.analysis.projects = [("default".into(), "API".into()), ("web".into(), "UI".into())].into();
        assert!(cfg.analysis_problems().is_empty());
        for id in ["default", "web"] {
            let project = cfg.for_project(id, &paths).unwrap();
            assert!(!project.analysis.enabled);
            assert!(project.analysis_problems().is_empty());
        }
    }
}
