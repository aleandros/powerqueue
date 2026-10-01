//! Parser and evaluator for `PRIORITY.md`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{Criticality, ModelTier, Task};

/// A parse problem with a line number, surfaced by `priority check` and `doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("PRIORITY.md line {line}: {message}")]
pub struct RuleError {
    pub line: usize,
    pub message: String,
}

/// One predicate on a task field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Condition {
    /// `field: value` — case-insensitive equality (labels: membership).
    Equals { field: String, value: String },
    /// `field != value`
    NotEquals { field: String, value: String },
    /// `field ~ regex` — case-insensitive regex match.
    Matches { field: String, pattern: String },
    /// `field > n`
    GreaterThan { field: String, value: f64 },
    /// `field < n`
    LessThan { field: String, value: f64 },
}

/// A rule under a criticality section: all conditions must hold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub line: usize,
    pub conditions: Vec<Condition>,
}

/// `+N if <conditions>` / `-N if <conditions>` under `## Scoring`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoringRule {
    pub line: usize,
    pub delta: f64,
    pub conditions: Vec<Condition>,
}

/// Per-ticket pin under `## Overrides`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Override {
    Criticality(Criticality),
    Score(f64),
    Model(ModelTier),
    /// Never schedule this ticket.
    Skip,
}

/// `## Jev` settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevSection {
    pub enabled: bool,
    pub question: String,
    pub levels: Vec<String>,
}

impl Default for JevSection {
    fn default() -> Self {
        Self {
            enabled: false,
            question: "How important is it to ship this ticket this week?".to_string(),
            levels: vec![
                "can wait indefinitely".into(),
                "nice to have".into(),
                "important this week".into(),
                "blocking customers or revenue".into(),
            ],
        }
    }
}

/// The parsed document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriorityRules {
    pub sections: BTreeMap<Criticality, Vec<Rule>>,
    pub default_criticality: Criticality,
    pub scoring: Vec<ScoringRule>,
    pub overrides: BTreeMap<String, Vec<Override>>,
    pub models: BTreeMap<Criticality, ModelTier>,
    pub jev: JevSection,
    /// Non-fatal problems found while parsing (unknown fields, odd lines).
    pub warnings: Vec<RuleError>,
}

impl Default for PriorityRules {
    fn default() -> Self {
        Self {
            sections: BTreeMap::new(),
            default_criticality: Criticality::Normal,
            scoring: Vec::new(),
            overrides: BTreeMap::new(),
            models: BTreeMap::new(),
            jev: JevSection::default(),
            warnings: Vec::new(),
        }
    }
}

/// Outcome of evaluating a task against the rules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    pub criticality: Criticality,
    pub score: f64,
    pub model: Option<ModelTier>,
    pub skip: bool,
    /// Human-readable trail: which rule set what.
    pub reasons: Vec<String>,
}

impl PriorityRules {
    /// Parse the Markdown document. Fatal errors (unparseable rules) are
    /// returned; recoverable issues land in `warnings`.
    pub fn parse(markdown: &str) -> Result<Self, Vec<RuleError>> {
        let _ = markdown;
        todo!("TODO(agent-linear): parse sections, rules, scoring, overrides, models, jev")
    }

    /// Evaluate a task. `jev_normalized` (0..=1) and `jev_weight` fold an
    /// optional Jev score into the result; `age_boost_per_hour` rewards
    /// waiting tasks so nothing starves.
    pub fn evaluate(&self, task: &Task, now: DateTime<Utc>, jev_normalized: Option<f64>, jev_weight: f64, age_boost_per_hour: f64) -> Evaluation {
        let _ = (self, task, now, jev_normalized, jev_weight, age_boost_per_hour);
        todo!("TODO(agent-linear)")
    }

    /// Model the rules assign to a criticality, if any.
    pub fn model_for(&self, criticality: Criticality) -> Option<ModelTier> {
        self.models.get(&criticality).copied()
    }
}

/// Evaluate a single condition against a task (exposed for tests and `priority explain`).
pub fn condition_matches(cond: &Condition, task: &Task) -> bool {
    let _ = (cond, task);
    todo!("TODO(agent-linear)")
}
