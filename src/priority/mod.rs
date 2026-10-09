//! Priority rules from `PRIORITY.md`.
//!
//! The file is ordinary Markdown so it stays readable in any editor and can
//! live next to other notes. Sections are `##` headings; rules are bullet
//! lines. See [`template`] for the reference document and `docs/priority.md`
//! for the grammar.
//!
//! ```markdown
//! ## Critical
//! - label: incident
//! - priority: urgent and label: customer
//!
//! ## High
//! - priority: high
//! - project ~ "Launch"
//!
//! ## Low
//! - label: chore
//!
//! ## Scoring
//! - +50 if label: customer
//! - -20 if estimate > 8
//!
//! ## Overrides
//! - ENG-123: critical
//! - ENG-200: +100
//! - ENG-201: model = opus
//!
//! ## Models
//! - if label: model/fable: fable
//! - critical: fable
//! - high: opus
//! - normal: sonnet
//! - low: sonnet
//!
//! ## Jev
//! - enabled: true
//! - question: How urgent is this ticket for a small team shipping weekly?
//! - levels: trivial | nice to have | important | blocking
//! ```

pub mod rules;
pub mod watcher;

pub use rules::{Condition, Evaluation, JevSection, ModelRule, PriorityRules, Rule, RuleError, ScoringRule, fmt_conditions};
pub use watcher::RulesWatcher;

impl PriorityRules {
    /// Read and parse the rules from where [`crate::config::Config::rules_source`]
    /// points: `Ok(None)` when there is no rules file there, an error naming
    /// the source and every offending line when it does not parse or cannot
    /// be read. The one place the CLI formats rule errors, so `budget`,
    /// `priority explain` and `tune` say the same thing.
    pub fn from_source(source: &crate::config::RulesSource) -> anyhow::Result<Option<Self>> {
        let Some(text) = source.read()? else { return Ok(None) };
        Self::parse(&text).map(Some).map_err(|errors| anyhow::anyhow!("{source} has errors:\n  - {}", format_errors(&errors)))
    }
}

/// `line N: message` per error, one per line, for messages that already
/// name the file.
pub fn format_errors(errors: &[RuleError]) -> String {
    errors.iter().map(|e| format!("line {}: {}", e.line, e.message)).collect::<Vec<_>>().join("\n  - ")
}

/// The default `PRIORITY.md` written by `powerqueue init`.
pub fn template() -> &'static str {
    include_str!("PRIORITY.template.md")
}
