//! Parser and evaluator for `PRIORITY.md`.
//!
//! The parser is line based: only `## Heading` lines and `- bullet` lines
//! matter; prose, `# h1` headings, blank lines and HTML comments are ignored.
//! Fatal problems (a rule that cannot be understood) are reported with their
//! line number; cosmetic ones (unknown sections, repeated settings) become
//! warnings on the parsed document.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::Path;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{Criticality, ModelTier, Task, TaskSource};

/// A parse problem with a line number, surfaced by `priority check` and `doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("PRIORITY.md line {line}: {message}")]
pub struct RuleError {
    pub line: usize,
    pub message: String,
}

impl RuleError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self { line, message: message.into() }
    }
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

impl Condition {
    /// The task field this condition inspects.
    pub fn field(&self) -> &str {
        match self {
            Condition::Equals { field, .. }
            | Condition::NotEquals { field, .. }
            | Condition::Matches { field, .. }
            | Condition::GreaterThan { field, .. }
            | Condition::LessThan { field, .. } => field,
        }
    }
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Condition::Equals { field, value } => write!(f, "{field}: {value}"),
            Condition::NotEquals { field, value } => write!(f, "{field} != {value}"),
            Condition::Matches { field, pattern } => write!(f, "{field} ~ {pattern}"),
            Condition::GreaterThan { field, value } => write!(f, "{field} > {}", fmt_num(*value)),
            Condition::LessThan { field, value } => write!(f, "{field} < {}", fmt_num(*value)),
        }
    }
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

/// `if <conditions>: <model> [| <model>...]` under `## Models`: when every
/// condition holds, these models win over the criticality row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRule {
    pub line: usize,
    pub conditions: Vec<Condition>,
    /// Preferred models, most wanted first.
    pub models: Vec<ModelTier>,
}

impl fmt::Display for ModelRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "if {}: {}", fmt_conditions(&self.conditions), fmt_models(&self.models))
    }
}

/// Per-ticket pin under `## Overrides`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Override {
    Criticality(Criticality),
    Score(f64),
    /// Preferred models, most wanted first (`KEY: model = fable | gpt-6.1-sol`).
    Model(Vec<ModelTier>),
    /// Never schedule this ticket.
    Skip,
}

impl fmt::Display for Override {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Override::Criticality(c) => write!(f, "{c}"),
            Override::Score(d) => write!(f, "{}", fmt_delta(*d)),
            Override::Model(m) => write!(f, "model = {}", fmt_models(m)),
            Override::Skip => f.write_str("skip"),
        }
    }
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
    /// Keyed by lower-cased task key.
    pub overrides: BTreeMap<String, Vec<Override>>,
    /// `## Models`: preferred models per criticality, most wanted first.
    /// Alternatives may belong to different providers.
    pub models: BTreeMap<Criticality, Vec<ModelTier>>,
    /// `## Models` conditional rows (`if label: model/fable: fable`), in
    /// file order; the first that matches beats the criticality row.
    #[serde(default)]
    pub model_rules: Vec<ModelRule>,
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
            model_rules: Vec::new(),
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
    /// The first entry of `models` (kept for callers that want one model).
    pub model: Option<ModelTier>,
    /// Preferred models, most wanted first: the `## Overrides` model list,
    /// else the first matching `## Models` `if` row, else the `## Models`
    /// entry for the criticality, else empty. The scheduler hands this list
    /// to the budget policy.
    pub models: Vec<ModelTier>,
    /// Which line chose `models`: `## Overrides`, `if <conditions>` or
    /// `<criticality> row`; `None` when `models` is empty.
    #[serde(default)]
    pub model_source: Option<String>,
    pub skip: bool,
    /// Human-readable trail: which rule set what.
    pub reasons: Vec<String>,
}

/// Maximum score a task can gain from waiting.
pub const MAX_AGE_BOOST: f64 = 200.0;

/// Fields a condition may reference.
const FIELDS: [&str; 11] =
    ["label", "priority", "estimate", "project", "cycle", "cycle_number", "team", "title", "description", "source", "key"];
/// Fields that support `>` / `<`.
const NUMERIC_FIELDS: [&str; 3] = ["priority", "estimate", "cycle_number"];
/// Values `cycle` may be compared with (the status word stored on the task).
const CYCLE_WORDS: [&str; 4] = ["active", "next", "past", "future"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Criticality(Criticality),
    Default,
    Scoring,
    Overrides,
    Models,
    Jev,
    Unknown,
}

impl PriorityRules {
    /// Read and parse `path`. Missing file or parse errors are a single
    /// `anyhow` error listing every problem with its line number.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&text).map_err(|errors| {
            let list: Vec<String> = errors.iter().map(|e| format!("  - line {}: {}", e.line, e.message)).collect();
            anyhow::anyhow!("{} has {} problem(s):\n{}", path.display(), errors.len(), list.join("\n"))
        })
    }

    /// Parse the Markdown document. Fatal errors (unparseable rules) are
    /// returned; recoverable issues land in `warnings`.
    pub fn parse(markdown: &str) -> Result<Self, Vec<RuleError>> {
        let mut rules = PriorityRules::default();
        let mut errors: Vec<RuleError> = Vec::new();
        let mut section: Option<Section> = None;
        let mut default_seen: Option<usize> = None;
        let mut in_comment = false;

        for (idx, raw) in markdown.lines().enumerate() {
            let line_no = idx + 1;
            let line = strip_comments(raw, &mut in_comment);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(heading) = line.strip_prefix("## ") {
                let name = heading.trim().trim_end_matches('#').trim();
                section = Some(match name.to_ascii_lowercase().as_str() {
                    "critical" => Section::Criticality(Criticality::Critical),
                    "high" => Section::Criticality(Criticality::High),
                    "normal" => Section::Criticality(Criticality::Normal),
                    "low" => Section::Criticality(Criticality::Low),
                    "default" => Section::Default,
                    "scoring" => Section::Scoring,
                    "overrides" => Section::Overrides,
                    "models" => Section::Models,
                    "jev" => Section::Jev,
                    _ => {
                        rules.warnings.push(RuleError::new(line_no, format!("unknown section `## {name}` is ignored")));
                        Section::Unknown
                    }
                });
                continue;
            }
            if line.starts_with('#') {
                // h1 / h3+ headings are documentation.
                continue;
            }
            let Some(bullet) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).map(str::trim) else {
                continue; // prose
            };
            if bullet.is_empty() {
                continue;
            }
            let Some(current) = section else {
                rules.warnings.push(RuleError::new(line_no, "bullet before any `##` section is ignored"));
                continue;
            };
            match current {
                Section::Unknown => {}
                Section::Criticality(c) => match parse_conditions(bullet, line_no) {
                    Ok(conditions) => rules.sections.entry(c).or_default().push(Rule { line: line_no, conditions }),
                    Err(e) => errors.push(e),
                },
                Section::Default => match bullet.parse::<Criticality>() {
                    Ok(c) => {
                        if let Some(prev) = default_seen {
                            rules.warnings.push(RuleError::new(
                                line_no,
                                format!("## Default already set on line {prev}; this bullet is ignored"),
                            ));
                        } else {
                            rules.default_criticality = c;
                            default_seen = Some(line_no);
                        }
                    }
                    Err(e) => errors.push(RuleError::new(line_no, format!("## Default: {e}"))),
                },
                Section::Scoring => match parse_scoring(bullet, line_no) {
                    Ok(rule) => rules.scoring.push(rule),
                    Err(e) => errors.push(e),
                },
                Section::Overrides => match parse_override(bullet, line_no, &mut rules.warnings) {
                    Ok((key, ov)) => rules.overrides.entry(key).or_default().push(ov),
                    Err(e) => errors.push(e),
                },
                Section::Models if is_conditional_model_line(bullet) => {
                    match parse_conditional_model_line(bullet, line_no, &mut rules.warnings) {
                        Ok(rule) => {
                            let key = condition_key(&rule.conditions);
                            if let Some(prev) = rules.model_rules.iter().find(|r| condition_key(&r.conditions) == key) {
                                rules.warnings.push(RuleError::new(
                                    line_no,
                                    format!(
                                        "## Models: `if {}` repeats line {}; the first matching row wins, so this one never applies",
                                        fmt_conditions(&rule.conditions),
                                        prev.line
                                    ),
                                ));
                            }
                            rules.model_rules.push(rule);
                        }
                        Err(e) => errors.push(e),
                    }
                }
                Section::Models => match parse_model_line(bullet, line_no, &mut rules.warnings) {
                    Ok((c, m)) => {
                        if rules.models.insert(c, m).is_some() {
                            rules.warnings.push(RuleError::new(line_no, format!("## Models: `{c}` set twice; last one wins")));
                        }
                    }
                    Err(e) => errors.push(e),
                },
                Section::Jev => {
                    if let Err(e) = parse_jev_line(bullet, line_no, &mut rules) {
                        errors.push(e);
                    }
                }
            }
        }

        if in_comment {
            rules.warnings.push(RuleError::new(markdown.lines().count().max(1), "unterminated HTML comment"));
        }
        if errors.is_empty() { Ok(rules) } else { Err(errors) }
    }

    /// Evaluate a task. `jev_normalized` (0..=1) and `jev_weight` fold an
    /// optional Jev score into the result; `age_boost_per_hour` rewards
    /// waiting tasks so nothing starves (capped at [`MAX_AGE_BOOST`]).
    ///
    /// Precedence: `## Overrides` (skip / criticality / model) beat the
    /// criticality sections, which are tried in order Critical → High →
    /// Normal → Low; the first matching rule wins; otherwise `## Default`.
    /// Models: `## Overrides`, then the first matching `## Models` `if`
    /// row, then the `## Models` row for the criticality.
    pub fn evaluate(
        &self,
        task: &Task,
        now: DateTime<Utc>,
        jev_normalized: Option<f64>,
        jev_weight: f64,
        age_boost_per_hour: f64,
    ) -> Evaluation {
        let mut reasons: Vec<String> = Vec::new();
        let mut skip = false;
        let mut criticality: Option<Criticality> = None;
        let mut models: Vec<ModelTier> = Vec::new();
        let mut model_source: Option<String> = None;
        let mut override_delta = 0.0;

        if let Some(overrides) = self.overrides.get(&task.key.to_ascii_lowercase()) {
            for ov in overrides {
                match ov {
                    Override::Skip => {
                        skip = true;
                        reasons.push(format!("skip: override for {}", task.key));
                    }
                    Override::Criticality(c) => {
                        criticality = Some(*c);
                        reasons.push(format!("{c}: override for {}", task.key));
                    }
                    Override::Score(d) => {
                        override_delta += d;
                        reasons.push(format!("{} override for {}", fmt_delta(*d), task.key));
                    }
                    Override::Model(m) => {
                        models = m.clone();
                        model_source = Some("## Overrides".to_string());
                        reasons.push(format!("model {} from ## Overrides", fmt_models(m)));
                    }
                }
            }
        }

        if criticality.is_none() {
            'sections: for c in Criticality::ALL {
                for rule in self.sections.get(&c).map(Vec::as_slice).unwrap_or(&[]) {
                    if rule.conditions.iter().all(|cond| condition_matches(cond, task)) {
                        criticality = Some(c);
                        reasons.push(format!("{c}: matched rule at line {} ({})", rule.line, fmt_conditions(&rule.conditions)));
                        break 'sections;
                    }
                }
            }
        }
        let criticality = criticality.unwrap_or_else(|| {
            reasons.push(format!("{}: default (no rule matched)", self.default_criticality));
            self.default_criticality
        });

        let mut score = criticality.base_score();
        reasons.push(format!("base {} ({criticality})", fmt_num(score)));
        for rule in &self.scoring {
            if rule.conditions.iter().all(|cond| condition_matches(cond, task)) {
                score += rule.delta;
                reasons.push(format!(
                    "{} scoring line {} ({})",
                    fmt_delta(rule.delta),
                    rule.line,
                    fmt_conditions(&rule.conditions)
                ));
            }
        }
        score += override_delta;

        if let Some(jev) = jev_normalized {
            let jev = jev.clamp(0.0, 1.0);
            let contribution = jev * jev_weight;
            score += contribution;
            reasons.push(format!("jev {jev:.2} × {} = {}", fmt_num(jev_weight), fmt_delta(contribution)));
        }

        if age_boost_per_hour > 0.0 {
            let hours = (now - task.created_at).num_seconds().max(0) as f64 / 3600.0;
            let boost = (hours * age_boost_per_hour).min(MAX_AGE_BOOST);
            if boost > 0.0 {
                score += boost;
                reasons.push(format!("age +{boost:.1} ({hours:.1}h)"));
            }
        }

        if models.is_empty()
            && let Some(rule) = self.model_rules.iter().find(|r| r.conditions.iter().all(|cond| condition_matches(cond, task)))
        {
            models = rule.models.clone();
            let source = format!("if {}", fmt_conditions(&rule.conditions));
            reasons.push(format!("model {} from ## Models line {} ({source})", fmt_models(&models), rule.line));
            model_source = Some(source);
        }

        if models.is_empty()
            && let Some(m) = self.models.get(&criticality)
        {
            models = m.clone();
            model_source = Some(format!("{criticality} row"));
            reasons.push(format!("model {} from ## Models", fmt_models(m)));
        }

        Evaluation { criticality, score, model: models.first().cloned(), models, model_source, skip, reasons }
    }

    /// Models the rules prefer for a criticality, most wanted first; empty
    /// when `## Models` says nothing about it.
    pub fn model_for(&self, criticality: Criticality) -> &[ModelTier] {
        self.models.get(&criticality).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Number of rules under the criticality sections.
    pub fn rule_count(&self) -> usize {
        self.sections.values().map(Vec::len).sum()
    }

    /// Readable multi-line summary of the parsed document (for `priority show`).
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for c in Criticality::ALL {
            let rules = self.sections.get(&c).map(Vec::as_slice).unwrap_or(&[]);
            out.push_str(&format!("{} ({} rule{})\n", capitalize(c.as_str()), rules.len(), plural(rules.len())));
            for r in rules {
                out.push_str(&format!("  line {:>3}: {}\n", r.line, fmt_conditions(&r.conditions)));
            }
        }
        out.push_str(&format!("Default: {}\n", self.default_criticality));
        out.push_str(&format!("Scoring ({} rule{})\n", self.scoring.len(), plural(self.scoring.len())));
        for s in &self.scoring {
            out.push_str(&format!("  line {:>3}: {} if {}\n", s.line, fmt_delta(s.delta), fmt_conditions(&s.conditions)));
        }
        out.push_str(&format!("Overrides ({})\n", self.overrides.len()));
        for (key, ovs) in &self.overrides {
            let list: Vec<String> = ovs.iter().map(|o| o.to_string()).collect();
            out.push_str(&format!("  {}: {}\n", key.to_ascii_uppercase(), list.join(", ")));
        }
        out.push_str("Models\n");
        for r in &self.model_rules {
            out.push_str(&format!("  line {:>3}: {r}\n", r.line));
        }
        for c in Criticality::ALL {
            match self.models.get(&c) {
                Some(m) => out.push_str(&format!("  {c}: {}\n", fmt_models(m))),
                None => out.push_str(&format!("  {c}: (budget policy decides)\n")),
            }
        }
        out.push_str(&format!("Jev: {}\n", if self.jev.enabled { "enabled" } else { "disabled" }));
        out.push_str(&format!("  question: {}\n", self.jev.question));
        out.push_str(&format!("  levels: {}\n", self.jev.levels.join(" | ")));
        if !self.warnings.is_empty() {
            out.push_str(&format!("Warnings ({})\n", self.warnings.len()));
            for w in &self.warnings {
                out.push_str(&format!("  line {:>3}: {}\n", w.line, w.message));
            }
        }
        out
    }
}

/// Evaluate a single condition against a task (exposed for tests and `priority explain`).
///
/// A missing optional field (`priority`, `estimate`, `project`, `cycle`,
/// `cycle_number`, `team`) never matches, except for `!=`, which does.
pub fn condition_matches(cond: &Condition, task: &Task) -> bool {
    match cond {
        Condition::Equals { field, value } => field_equals(field, value, task).unwrap_or(false),
        Condition::NotEquals { field, value } => !field_equals(field, value, task).unwrap_or(false),
        Condition::Matches { field, pattern } => {
            let Some(re) = compile_regex(pattern) else { return false };
            if field == "label" {
                // The qualified form (`model/fable`) and, so regexes written
                // before labels were qualified keep matching, the bare child name.
                return task.labels.iter().any(|l| {
                    re.is_match(l)
                        || l.rsplit_once(crate::domain::LABEL_PARENT_SEPARATOR).is_some_and(|(_, child)| re.is_match(child))
                });
            }
            field_text(field, task).map(|t| re.is_match(&t)).unwrap_or(false)
        }
        Condition::GreaterThan { field, value } => field_number(field, task).map(|n| n > *value).unwrap_or(false),
        Condition::LessThan { field, value } => field_number(field, task).map(|n| n < *value).unwrap_or(false),
    }
}

/// `Some(true/false)` if the field is present; `None` if it is missing on this task.
fn field_equals(field: &str, value: &str, task: &Task) -> Option<bool> {
    match field {
        "label" => Some(task.labels.iter().any(|l| crate::domain::label_matches(l, value))),
        "priority" => {
            let wanted = parse_priority_value(value).ok()?;
            task.linear_priority.map(|p| f64::from(p) == wanted)
        }
        "estimate" => {
            let wanted = value.trim().parse::<f64>().ok()?;
            task.estimate.map(|e| (e - wanted).abs() < 1e-9)
        }
        "cycle_number" => {
            let wanted = value.trim().parse::<f64>().ok()?;
            task.cycle_number.map(|n| (f64::from(n) - wanted).abs() < 1e-9)
        }
        _ => field_text(field, task).map(|t| t.trim().eq_ignore_ascii_case(value.trim())),
    }
}

/// Textual value of a field, `None` when the task has no value for it.
fn field_text(field: &str, task: &Task) -> Option<String> {
    match field {
        "label" => Some(task.labels.join(", ")),
        "priority" => task.linear_priority.map(|p| priority_name(p).to_string()),
        "estimate" => task.estimate.map(fmt_num),
        "project" => task.project.clone(),
        "cycle" => task.cycle.clone(),
        "cycle_number" => task.cycle_number.map(|n| n.to_string()),
        "team" => match &task.source {
            TaskSource::Linear { team_key, .. } => Some(team_key.clone()),
            TaskSource::Manual | TaskSource::GitHub { .. } => None,
        },
        "title" => Some(task.title.clone()),
        "description" => Some(task.description.clone()),
        "source" => Some(task.source.kind().to_string()),
        "key" => Some(task.key.clone()),
        _ => None,
    }
}

fn field_number(field: &str, task: &Task) -> Option<f64> {
    match field {
        "priority" => task.linear_priority.map(f64::from),
        "estimate" => task.estimate,
        "cycle_number" => task.cycle_number.map(f64::from),
        _ => None,
    }
}

thread_local! {
    static REGEX_CACHE: RefCell<HashMap<String, Option<Regex>>> = RefCell::new(HashMap::new());
}

/// Case-insensitive regex, cached per thread. `None` if the pattern is invalid
/// (the parser rejects such patterns, so this only happens for hand-built conditions).
fn compile_regex(pattern: &str) -> Option<Regex> {
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(hit) = cache.get(pattern) {
            return hit.clone();
        }
        let compiled = build_regex(pattern).ok();
        if cache.len() > 512 {
            cache.clear();
        }
        cache.insert(pattern.to_string(), compiled.clone());
        compiled
    })
}

fn build_regex(pattern: &str) -> Result<Regex, regex::Error> {
    regex::RegexBuilder::new(pattern).case_insensitive(true).size_limit(1 << 20).build()
}

// ------------------------------------------------------------------ parsing

/// Remove `<!-- ... -->` comments, possibly spanning lines.
fn strip_comments(line: &str, in_comment: &mut bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        if *in_comment {
            match rest.find("-->") {
                Some(end) => {
                    *in_comment = false;
                    rest = &rest[end + 3..];
                }
                None => return out,
            }
        } else {
            match rest.find("<!--") {
                Some(start) => {
                    out.push_str(&rest[..start]);
                    *in_comment = true;
                    rest = &rest[start + 4..];
                }
                None => {
                    out.push_str(rest);
                    return out;
                }
            }
        }
    }
}

/// Split on ` and ` (case-insensitive) and parse each part.
fn parse_conditions(text: &str, line: usize) -> Result<Vec<Condition>, RuleError> {
    let and_re = and_regex();
    let mut conditions = Vec::new();
    for part in and_re.split(text) {
        let part = part.trim();
        if part.is_empty() {
            return Err(RuleError::new(line, "empty condition next to `and`"));
        }
        conditions.push(parse_condition(part, line)?);
    }
    if conditions.is_empty() {
        return Err(RuleError::new(line, "expected a condition such as `label: incident`"));
    }
    Ok(conditions)
}

fn and_regex() -> Regex {
    thread_local! {
        static AND: Regex = Regex::new(r"(?i)\s+and\s+").expect("valid regex");
    }
    AND.with(Clone::clone)
}

fn condition_regex() -> Regex {
    thread_local! {
        static COND: Regex = Regex::new(r"^([A-Za-z_]+)\s*(!=|~|>|<|:|==|=)\s*(.*)$").expect("valid regex");
    }
    COND.with(Clone::clone)
}

fn parse_condition(text: &str, line: usize) -> Result<Condition, RuleError> {
    let re = condition_regex();
    let caps = re
        .captures(text)
        .ok_or_else(|| RuleError::new(line, format!("cannot parse condition `{text}` (expected `field: value`, `field ~ regex`, `field > n`, `field < n` or `field != value`)")))?;
    let field = caps[1].to_ascii_lowercase();
    let op = &caps[2];
    let value = unquote(caps[3].trim());

    if field == "criticality" {
        return Err(RuleError::new(line, "`criticality` is the output of the rules and cannot be used as a condition"));
    }
    if !FIELDS.contains(&field.as_str()) {
        return Err(RuleError::new(line, format!("unknown field `{field}` (expected one of {})", FIELDS.join(", "))));
    }
    if value.is_empty() {
        return Err(RuleError::new(line, format!("missing value after `{field} {op}`")));
    }

    match op {
        ":" | "=" | "==" => {
            validate_value(&field, &value, line)?;
            Ok(Condition::Equals { field, value })
        }
        "!=" => {
            validate_value(&field, &value, line)?;
            Ok(Condition::NotEquals { field, value })
        }
        "~" => {
            build_regex(&value)
                .map_err(|e| RuleError::new(line, format!("invalid regex `{value}`: {}", regex_reason(&e.to_string()))))?;
            Ok(Condition::Matches { field, pattern: value })
        }
        ">" | "<" => {
            if !NUMERIC_FIELDS.contains(&field.as_str()) {
                return Err(RuleError::new(
                    line,
                    format!("`{op}` needs a numeric field ({}), not `{field}`", NUMERIC_FIELDS.join(", ")),
                ));
            }
            let number = parse_number_for(&field, &value, line)?;
            Ok(if op == ">" {
                Condition::GreaterThan { field, value: number }
            } else {
                Condition::LessThan { field, value: number }
            })
        }
        _ => Err(RuleError::new(line, format!("unknown operator `{op}`"))),
    }
}

/// Equality values on numeric fields must be parseable now, not at evaluation.
fn validate_value(field: &str, value: &str, line: usize) -> Result<(), RuleError> {
    match field {
        "priority" | "estimate" | "cycle_number" => parse_number_for(field, value, line).map(|_| ()),
        "cycle" => {
            if CYCLE_WORDS.contains(&value.trim().to_ascii_lowercase().as_str()) {
                Ok(())
            } else {
                Err(RuleError::new(line, format!("`cycle` must be one of {}, not `{value}`", CYCLE_WORDS.join("|"))))
            }
        }
        "source" => {
            if matches!(value.to_ascii_lowercase().as_str(), "linear" | "github" | "manual") {
                Ok(())
            } else {
                Err(RuleError::new(line, format!("`source` must be `linear`, `github` or `manual`, not `{value}`")))
            }
        }
        _ => Ok(()),
    }
}

fn parse_number_for(field: &str, value: &str, line: usize) -> Result<f64, RuleError> {
    match field {
        "priority" => parse_priority_value(value).map_err(|m| RuleError::new(line, m)),
        _ => value.trim().parse::<f64>().map_err(|_| RuleError::new(line, format!("`{field}` needs a number, not `{value}`"))),
    }
}

/// Priority names (`urgent`, `high`, ...) or numbers 0..=4.
fn parse_priority_value(value: &str) -> Result<f64, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" | "no priority" => Ok(0.0),
        "urgent" => Ok(1.0),
        "high" => Ok(2.0),
        "normal" | "medium" => Ok(3.0),
        "low" => Ok(4.0),
        other => match other.parse::<f64>() {
            Ok(n) if (0.0..=4.0).contains(&n) => Ok(n),
            _ => Err(format!("`priority` must be urgent|high|normal|low|none or 0-4, not `{value}`")),
        },
    }
}

/// The word `PRIORITY.md` uses for a Linear priority number (`urgent`, `high`,
/// `normal`, `low`, `none`; `unknown` outside 0..=4).
pub fn priority_name(p: u8) -> &'static str {
    match p {
        0 => "none",
        1 => "urgent",
        2 => "high",
        3 => "normal",
        4 => "low",
        _ => "unknown",
    }
}

fn parse_scoring(text: &str, line: usize) -> Result<ScoringRule, RuleError> {
    let re = {
        thread_local! {
            static SCORE: Regex = Regex::new(r"(?i)^([+-]\s*\d+(?:\.\d+)?)\s+(?:if\s+)?(.+)$").expect("valid regex");
        }
        SCORE.with(Clone::clone)
    };
    let caps = re.captures(text).ok_or_else(|| {
        RuleError::new(
            line,
            format!("cannot parse scoring rule `{text}` (expected `+N if <conditions>` or `-N if <conditions>`)"),
        )
    })?;
    let delta: f64 = caps[1].replace(' ', "").parse().map_err(|_| RuleError::new(line, format!("bad number in `{text}`")))?;
    let conditions = parse_conditions(caps[2].trim(), line)?;
    Ok(ScoringRule { line, delta, conditions })
}

fn parse_override(text: &str, line: usize, warnings: &mut Vec<RuleError>) -> Result<(String, Override), RuleError> {
    let (key, value) = text
        .split_once(':')
        .ok_or_else(|| RuleError::new(line, format!("cannot parse override `{text}` (expected `KEY: critical|high|normal|low`, `KEY: +N`, `KEY: model = <model> [| <model>...]` or `KEY: skip`)")))?;
    let key = key.trim();
    if key.is_empty() || key.contains(char::is_whitespace) {
        return Err(RuleError::new(line, format!("override key `{key}` must be a single task key such as ENG-123")));
    }
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let ov = if lower == "skip" {
        Override::Skip
    } else if let Some(delta) = value.strip_prefix('+').or_else(|| value.strip_prefix('-')) {
        let n: f64 = delta.trim().parse().map_err(|_| RuleError::new(line, format!("bad score delta `{value}` for {key}")))?;
        Override::Score(if value.starts_with('-') { -n } else { n })
    } else if let Some(rest) = lower.strip_prefix("model") {
        let list = rest.trim_start().strip_prefix(['=', ':']).map(str::trim).unwrap_or("");
        let models = parse_model_list(list, line, &format!("{key}: model"), warnings)?;
        Override::Model(models)
    } else {
        let c = value.parse::<Criticality>().map_err(|_| {
            RuleError::new(line, format!("cannot parse override value `{value}` for {key} (expected critical|high|normal|low, +N, -N, model = <model> [| <model>...] or skip)"))
        })?;
        Override::Criticality(c)
    };
    Ok((key.to_ascii_lowercase(), ov))
}

fn parse_model_line(text: &str, line: usize, warnings: &mut Vec<RuleError>) -> Result<(Criticality, Vec<ModelTier>), RuleError> {
    let (c, m) = text.split_once(':').or_else(|| text.split_once('=')).ok_or_else(|| {
        RuleError::new(line, format!("cannot parse model line `{text}` (expected `<criticality>: <model> [| <model>...]`)"))
    })?;
    let c = c.trim().parse::<Criticality>().map_err(|e| RuleError::new(line, format!("## Models: {e}")))?;
    let models = parse_model_list(m, line, "## Models", warnings)?;
    Ok((c, models))
}

/// Order- and case-insensitive identity of a condition list, to spot
/// conditional `## Models` rows that repeat an earlier one.
fn condition_key(conds: &[Condition]) -> Vec<String> {
    let mut key: Vec<String> = conds.iter().map(|c| c.to_string().to_lowercase()).collect();
    key.sort();
    key.dedup();
    key
}

/// `## Models` bullets starting with the word `if` are conditional rows.
fn is_conditional_model_line(text: &str) -> bool {
    text.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("if ")) || text.eq_ignore_ascii_case("if")
}

/// `if <conditions>: <model> [| <model>...]`. Conditions use the grammar of
/// the criticality sections. Both halves may contain `:` (`title ~ a:b`,
/// `codex:gpt-6`), so every `:` is tried left to right and the first split
/// where the conditions and the model list both parse wins; when none does,
/// the error comes from the split at the last `:`.
fn parse_conditional_model_line(text: &str, line: usize, warnings: &mut Vec<RuleError>) -> Result<ModelRule, RuleError> {
    let usage = "expected `if <conditions>: <model> [| <model>...]`, e.g. `if label: model/fable: fable`";
    let body = text.get(2..).unwrap_or("").trim();
    let split = |i: usize, warnings: &mut Vec<RuleError>| -> Result<ModelRule, RuleError> {
        let (conds, list) = (body[..i].trim(), body[i + 1..].trim());
        if conds.is_empty() || list.is_empty() {
            return Err(RuleError::new(line, format!("## Models: cannot parse `{text}` ({usage})")));
        }
        let conditions =
            parse_conditions(conds, line).map_err(|e| RuleError::new(line, format!("## Models: {} ({usage})", e.message)))?;
        let models = parse_model_list(list, line, "## Models", warnings)?;
        Ok(ModelRule { line, conditions, models })
    };
    let colons: Vec<usize> = body.match_indices(':').map(|(i, _)| i).collect();
    let Some(&last) = colons.last() else {
        return Err(RuleError::new(line, format!("## Models: cannot parse `{text}` ({usage})")));
    };
    for &i in &colons[..colons.len() - 1] {
        let mut tentative = Vec::new();
        if let Ok(rule) = split(i, &mut tentative) {
            warnings.extend(tentative);
            return Ok(rule);
        }
    }
    split(last, warnings)
}

/// Parse `fable | gpt-6.1-sol | sonnet`: `|`-separated alternatives in
/// preference order, whitespace tolerant. Every name goes through
/// `ModelTier::from_str`, so an unknown name is an error naming the alias
/// rules; a repeated name is dropped with a warning. `context` prefixes the
/// messages (`## Models`, `ENG-1: model`).
fn parse_model_list(text: &str, line: usize, context: &str, warnings: &mut Vec<RuleError>) -> Result<Vec<ModelTier>, RuleError> {
    let mut models: Vec<ModelTier> = Vec::new();
    for part in text.split('|') {
        let part = part.trim();
        if part.is_empty() {
            return Err(RuleError::new(line, format!("{context}: empty model name next to `|` in `{}`", text.trim())));
        }
        let model = part.parse::<ModelTier>().map_err(|e| RuleError::new(line, format!("{context}: {e}")))?;
        if models.contains(&model) {
            warnings.push(RuleError::new(line, format!("{context}: `{model}` listed twice; the repeat is ignored")));
            continue;
        }
        models.push(model);
    }
    if models.is_empty() {
        return Err(RuleError::new(line, format!("{context}: expected a model name such as `fable` or `fable | gpt-6.1-sol`")));
    }
    Ok(models)
}

fn parse_jev_line(text: &str, line: usize, rules: &mut PriorityRules) -> Result<(), RuleError> {
    let (key, value) = text.split_once(':').ok_or_else(|| {
        RuleError::new(
            line,
            format!("cannot parse Jev setting `{text}` (expected `enabled: true|false`, `question: ...` or `levels: a | b | c`)"),
        )
    })?;
    let value = value.trim();
    match key.trim().to_ascii_lowercase().as_str() {
        "enabled" => {
            rules.jev.enabled = match value.to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" => true,
                "false" | "no" | "off" => false,
                other => return Err(RuleError::new(line, format!("Jev `enabled` must be true or false, not `{other}`"))),
            };
        }
        "question" | "instructions" => {
            if value.is_empty() {
                return Err(RuleError::new(line, "Jev `question` is empty"));
            }
            rules.jev.question = value.to_string();
        }
        "levels" | "criteria" => {
            let levels: Vec<String> = value.split('|').map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            if !(2..=10).contains(&levels.len()) {
                return Err(RuleError::new(
                    line,
                    format!("Jev `levels` needs 2 to 10 entries separated by `|`, found {}", levels.len()),
                ));
            }
            rules.jev.levels = levels;
        }
        other => rules.warnings.push(RuleError::new(line, format!("unknown Jev setting `{other}` is ignored"))),
    }
    Ok(())
}

// ---------------------------------------------------------------- helpers

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\''))) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// The `regex` crate's errors span several lines (pattern, caret, `error: ...`);
/// keep the last informative one.
fn regex_reason(s: &str) -> &str {
    s.lines().rev().map(str::trim).find(|l| !l.is_empty()).map(|l| l.strip_prefix("error: ").unwrap_or(l)).unwrap_or(s)
}

/// `fable | gpt-6.1-sol`: a preference list as written in the file.
pub fn fmt_models(models: &[ModelTier]) -> String {
    models.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(" | ")
}

/// `label: a and priority: high`: conditions as written in the file.
pub fn fmt_conditions(conds: &[Condition]) -> String {
    conds.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(" and ")
}

fn fmt_num(n: f64) -> String {
    let rounded = n.round();
    if (n - rounded).abs() < 1e-6 && n.abs() < 1e15 { format!("{}", rounded as i64) } else { format!("{n:.2}") }
}

fn fmt_delta(n: f64) -> String {
    if n >= 0.0 { format!("+{}", fmt_num(n)) } else { format!("-{}", fmt_num(-n)) }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn linear_task(key: &str) -> Task {
        Task::new(
            key,
            "Fix login outage",
            TaskSource::Linear {
                issue_id: format!("uuid-{key}"),
                identifier: key.to_string(),
                url: "https://linear.app/x".into(),
                team_key: "ENG".into(),
            },
        )
    }

    fn parse_ok(md: &str) -> PriorityRules {
        match PriorityRules::parse(md) {
            Ok(r) => r,
            Err(errs) => panic!("unexpected parse errors: {errs:?}"),
        }
    }

    fn parse_err(md: &str) -> Vec<RuleError> {
        match PriorityRules::parse(md) {
            Ok(r) => panic!("expected errors, got {r:?}"),
            Err(errs) => errs,
        }
    }

    fn eval(rules: &PriorityRules, task: &Task) -> Evaluation {
        rules.evaluate(task, task.created_at, None, 0.0, 0.0)
    }

    #[test]
    fn parses_template_without_errors_or_warnings() {
        let rules = parse_ok(crate::priority::template());
        assert_eq!(rules.sections[&Criticality::Critical].len(), 3);
        assert_eq!(rules.sections[&Criticality::High].len(), 2);
        assert_eq!(rules.sections[&Criticality::Normal].len(), 1);
        assert_eq!(rules.sections[&Criticality::Low].len(), 2);
        assert_eq!(rules.default_criticality, Criticality::Normal);
        assert_eq!(rules.scoring.len(), 4);
        assert!(rules.overrides.is_empty());
        assert_eq!(rules.models[&Criticality::Critical], vec![ModelTier::fable()]);
        assert!(!rules.jev.enabled);
        assert_eq!(rules.jev.levels.len(), 4);
        assert!(rules.warnings.is_empty(), "{:?}", rules.warnings);
    }

    #[test]
    fn empty_document_is_default() {
        let rules = parse_ok("# Just a title\n\nSome prose.\n");
        assert_eq!(rules, PriorityRules::default());
    }

    #[test]
    fn parses_every_condition_operator() {
        let rules = parse_ok(
            "## High\n- label: Customer and priority != low and title ~ \"^fix\" and estimate > 2 and estimate < 8 and team: eng\n",
        );
        let conds = &rules.sections[&Criticality::High][0].conditions;
        assert_eq!(conds.len(), 6);
        assert_eq!(conds[0], Condition::Equals { field: "label".into(), value: "Customer".into() });
        assert_eq!(conds[1], Condition::NotEquals { field: "priority".into(), value: "low".into() });
        assert_eq!(conds[2], Condition::Matches { field: "title".into(), pattern: "^fix".into() });
        assert_eq!(conds[3], Condition::GreaterThan { field: "estimate".into(), value: 2.0 });
        assert_eq!(conds[4], Condition::LessThan { field: "estimate".into(), value: 8.0 });
        assert_eq!(conds[5], Condition::Equals { field: "team".into(), value: "eng".into() });
        assert_eq!(rules.sections[&Criticality::High][0].line, 2);
    }

    #[test]
    fn condition_matching_semantics() {
        let mut t = linear_task("ENG-1");
        t.labels = vec!["Customer".into(), "bug".into()];
        t.linear_priority = Some(2);
        t.estimate = Some(5.0);
        t.project = None;
        let m = |c: &Condition| condition_matches(c, &t);
        assert!(m(&Condition::Equals { field: "label".into(), value: "customer".into() }));
        assert!(!m(&Condition::Equals { field: "label".into(), value: "chore".into() }));
        assert!(m(&Condition::NotEquals { field: "label".into(), value: "chore".into() }));
        assert!(m(&Condition::Equals { field: "priority".into(), value: "high".into() }));
        assert!(m(&Condition::Equals { field: "priority".into(), value: "2".into() }));
        assert!(m(&Condition::Matches { field: "priority".into(), pattern: "^hi".into() }));
        assert!(m(&Condition::GreaterThan { field: "estimate".into(), value: 4.0 }));
        assert!(!m(&Condition::LessThan { field: "estimate".into(), value: 4.0 }));
        assert!(m(&Condition::Matches { field: "title".into(), pattern: "LOGIN".into() }));
        assert!(m(&Condition::Matches { field: "label".into(), pattern: "^cust".into() }));
        assert!(m(&Condition::Equals { field: "team".into(), value: "ENG".into() }));
        assert!(m(&Condition::Equals { field: "source".into(), value: "Linear".into() }));
        assert!(m(&Condition::Equals { field: "key".into(), value: "eng-1".into() }));
        // Missing optional fields never match, except `!=`.
        assert!(!m(&Condition::Equals { field: "project".into(), value: "Launch".into() }));
        assert!(m(&Condition::NotEquals { field: "project".into(), value: "Launch".into() }));
        assert!(!m(&Condition::Matches { field: "project".into(), pattern: ".*".into() }));
        let manual = Task::new("manual-1", "x", TaskSource::Manual);
        assert!(!condition_matches(&Condition::Equals { field: "team".into(), value: "eng".into() }, &manual));
        assert!(!condition_matches(&Condition::GreaterThan { field: "estimate".into(), value: 0.0 }, &manual));
        assert!(!condition_matches(&Condition::Equals { field: "priority".into(), value: "none".into() }, &manual));
        assert!(condition_matches(&Condition::NotEquals { field: "priority".into(), value: "none".into() }, &manual));
        assert!(condition_matches(&Condition::Equals { field: "source".into(), value: "manual".into() }, &manual));
    }

    #[test]
    fn sections_are_tried_in_order_and_first_rule_wins() {
        let rules = parse_ok("## Low\n- label: chore\n\n## Critical\n- label: incident\n\n## High\n- label: chore\n");
        let mut t = linear_task("ENG-1");
        t.labels = vec!["chore".into(), "incident".into()];
        let e = eval(&rules, &t);
        assert_eq!(e.criticality, Criticality::Critical);
        assert_eq!(e.reasons[0], "critical: matched rule at line 5 (label: incident)");
        t.labels = vec!["chore".into()];
        assert_eq!(eval(&rules, &t).criticality, Criticality::High);
        t.labels = vec![];
        let e = eval(&rules, &t);
        assert_eq!(e.criticality, Criticality::Normal);
        assert_eq!(e.reasons[0], "normal: default (no rule matched)");
    }

    #[test]
    fn default_section_sets_fallback() {
        let rules = parse_ok("## Default\n- low\n");
        assert_eq!(rules.default_criticality, Criticality::Low);
        let e = eval(&rules, &linear_task("ENG-1"));
        assert_eq!(e.criticality, Criticality::Low);
        assert_eq!(e.score, Criticality::Low.base_score());
        let errs = parse_err("## Default\n- whatever\n");
        assert_eq!(errs[0].line, 2);
        let rules = parse_ok("## Default\n- low\n- high\n");
        assert_eq!(rules.default_criticality, Criticality::Low);
        assert_eq!(rules.warnings.len(), 1);
    }

    #[test]
    fn scoring_rules_add_up_with_reasons() {
        let rules =
            parse_ok("## Scoring\n- +40 if label: customer\n- -30 if estimate > 8\n- +10 source: linear\n- +5 if label: nope\n");
        let mut t = linear_task("ENG-1");
        t.labels = vec!["customer".into()];
        t.estimate = Some(13.0);
        let e = eval(&rules, &t);
        assert_eq!(e.score, 100.0 + 40.0 - 30.0 + 10.0);
        assert!(e.reasons.contains(&"+40 scoring line 2 (label: customer)".to_string()), "{:?}", e.reasons);
        assert!(e.reasons.contains(&"-30 scoring line 3 (estimate > 8)".to_string()), "{:?}", e.reasons);
        assert!(e.reasons.contains(&"+10 scoring line 4 (source: linear)".to_string()), "{:?}", e.reasons);
        assert!(!e.reasons.iter().any(|r| r.contains("line 5")));
    }

    #[test]
    fn overrides_take_precedence() {
        let rules = parse_ok(
            "## Critical\n- label: incident\n\n## Overrides\n- eng-1: low\n- ENG-1: +100\n- ENG-2: model = opus\n- ENG-3: skip\n- ENG-4: -25\n\n## Models\n- critical: fable\n- low: haiku\n",
        );
        let mut t = linear_task("ENG-1");
        t.labels = vec!["incident".into()];
        let e = eval(&rules, &t);
        assert_eq!(e.criticality, Criticality::Low, "{:?}", e.reasons);
        assert_eq!(e.score, Criticality::Low.base_score() + 100.0);
        assert_eq!(e.model, Some(ModelTier::haiku()));
        assert!(e.reasons.contains(&"low: override for ENG-1".to_string()));
        assert!(e.reasons.contains(&"+100 override for ENG-1".to_string()));

        let mut t2 = linear_task("ENG-2");
        t2.labels = vec!["incident".into()];
        let e2 = eval(&rules, &t2);
        assert_eq!(e2.criticality, Criticality::Critical);
        assert_eq!(e2.model, Some(ModelTier::opus()));
        assert!(e2.reasons.contains(&"model opus from ## Overrides".to_string()));

        let e3 = eval(&rules, &linear_task("ENG-3"));
        assert!(e3.skip);
        assert!(!eval(&rules, &linear_task("ENG-2")).skip);

        let e4 = eval(&rules, &linear_task("ENG-4"));
        assert_eq!(e4.score, Criticality::Normal.base_score() - 25.0);
    }

    #[test]
    fn models_section_picks_model_by_criticality() {
        let rules = parse_ok("## High\n- priority: high\n\n## Models\n- high: opus\n- normal: sonnet\n");
        let mut t = linear_task("ENG-1");
        t.linear_priority = Some(2);
        let e = eval(&rules, &t);
        assert_eq!(e.model, Some(ModelTier::opus()));
        assert!(e.reasons.contains(&"model opus from ## Models".to_string()));
        t.linear_priority = Some(3);
        assert_eq!(eval(&rules, &t).model, Some(ModelTier::sonnet()));
        let no_models = parse_ok("## High\n- priority: high\n");
        assert_eq!(eval(&no_models, &t).model, None);
        assert!(rules.model_for(Criticality::Low).is_empty());
    }

    #[test]
    fn model_lists_are_preference_ordered_and_cross_providers() {
        let rules = parse_ok(
            "## Critical\n- label: incident\n\n## Overrides\n- ENG-5: model = gpt-6-astra | sonnet\n\n## Models\n- critical: fable | gpt-6.1-sol\n- high = opus|gemini-3-pro | haiku\n",
        );
        assert!(rules.warnings.is_empty(), "{:?}", rules.warnings);
        assert_eq!(rules.models[&Criticality::Critical], vec![ModelTier::fable(), ModelTier::new("gpt-6.1-sol")]);
        assert_eq!(rules.model_for(Criticality::High), &[ModelTier::opus(), ModelTier::new("gemini-3-pro"), ModelTier::haiku()]);
        assert!(rules.model_for(Criticality::Low).is_empty());
        let mut t = linear_task("ENG-1");
        t.labels = vec!["incident".into()];
        let e = eval(&rules, &t);
        assert_eq!(e.models, vec![ModelTier::fable(), ModelTier::new("gpt-6.1-sol")]);
        assert_eq!(e.model, Some(ModelTier::fable()), "`model` is the first alternative");
        assert!(e.reasons.contains(&"model fable | gpt-6.1-sol from ## Models".to_string()), "{:?}", e.reasons);
        let e = eval(&rules, &linear_task("ENG-5"));
        assert_eq!(e.models, vec![ModelTier::new("gpt-6-astra"), ModelTier::sonnet()]);
        assert!(e.reasons.contains(&"model gpt-6-astra | sonnet from ## Overrides".to_string()), "{:?}", e.reasons);
        assert_eq!(rules.overrides["eng-5"][0].to_string(), "model = gpt-6-astra | sonnet");
        let text = rules.describe();
        assert!(text.contains("critical: fable | gpt-6.1-sol"), "{text}");
    }

    #[test]
    fn conditional_model_rows_parse() {
        let rules = parse_ok(
            "## Models\n- if label: model/fable: fable\n- IF label ~ ^model/ and priority: urgent: opus | gpt-6.1-sol\n- critical: sonnet\n",
        );
        assert!(rules.warnings.is_empty(), "{:?}", rules.warnings);
        assert_eq!(rules.models[&Criticality::Critical], vec![ModelTier::sonnet()]);
        assert_eq!(rules.model_rules.len(), 2);
        assert_eq!(
            rules.model_rules[0],
            ModelRule {
                line: 2,
                conditions: vec![Condition::Equals { field: "label".into(), value: "model/fable".into() }],
                models: vec![ModelTier::fable()],
            }
        );
        let second = &rules.model_rules[1];
        assert_eq!(second.line, 3);
        assert_eq!(second.conditions[0], Condition::Matches { field: "label".into(), pattern: "^model/".into() });
        assert_eq!(second.conditions[1], Condition::Equals { field: "priority".into(), value: "urgent".into() });
        assert_eq!(second.models, vec![ModelTier::opus(), ModelTier::new("gpt-6.1-sol")]);
        assert_eq!(second.to_string(), "if label ~ ^model/ and priority: urgent: opus | gpt-6.1-sol");
        let text = rules.describe();
        assert!(text.contains("line   2: if label: model/fable: fable"), "{text}");
    }

    #[test]
    fn conditional_model_rows_split_around_colons_in_either_half() {
        let rules = parse_ok(
            "## Models\n- if label: x: claude:opus\n- if label: y: fable | codex:gpt-6\n- if title ~ a:b: fable\n- if label: codex: opus\n",
        );
        let r = &rules.model_rules;
        assert_eq!(r[0].conditions, vec![Condition::Equals { field: "label".into(), value: "x".into() }]);
        assert_eq!(r[0].models, vec![ModelTier::opus()]);
        assert_eq!(r[1].conditions, vec![Condition::Equals { field: "label".into(), value: "y".into() }]);
        assert_eq!(r[1].models, vec![ModelTier::fable(), "codex:gpt-6".parse::<ModelTier>().unwrap()]);
        assert_eq!(r[2].conditions, vec![Condition::Matches { field: "title".into(), pattern: "a:b".into() }]);
        assert_eq!(r[2].models, vec![ModelTier::fable()]);
        assert_eq!(r[3].conditions, vec![Condition::Equals { field: "label".into(), value: "codex".into() }]);
        assert_eq!(r[3].models, vec![ModelTier::opus()]);
        assert!(rules.warnings.is_empty(), "{:?}", rules.warnings);
    }

    #[test]
    fn repeated_conditional_rows_warn() {
        let rules = parse_ok(
            "## Models\n- if label: a and priority: high: fable\n- if priority: HIGH and label: A: opus\n- if label: b: opus\n",
        );
        assert_eq!(rules.model_rules.len(), 3);
        assert_eq!(rules.warnings.len(), 1, "{:?}", rules.warnings);
        assert_eq!(rules.warnings[0].line, 3);
        assert!(rules.warnings[0].message.contains("repeats line 2"), "{}", rules.warnings[0].message);
    }

    #[test]
    fn conditional_model_row_errors() {
        let errs = parse_err(
            "## Models\n- if label: model/fable\n- if: fable\n- if colour: red: fable\n- if label: x: llama\n- if label: x:\n",
        );
        let lines: Vec<usize> = errs.iter().map(|e| e.line).collect();
        assert_eq!(lines, vec![2, 3, 4, 5, 6], "{errs:?}");
        assert!(errs[0].message.contains("if <conditions>: <model>"), "{}", errs[0].message);
        assert!(errs[2].message.contains("unknown field `colour`"), "{}", errs[2].message);
        assert!(errs[3].message.contains("unknown model `llama`"), "{}", errs[3].message);
    }

    #[test]
    fn first_matching_conditional_row_beats_criticality_row() {
        let rules = parse_ok(
            "## High\n- priority: high\n\n## Overrides\n- ENG-9: model = haiku\n\n## Models\n- if label: model/fable: fable\n- if label: model/fable: opus\n- if label ~ ^model/: sonnet\n- high: opus\n",
        );
        let mut t = linear_task("ENG-1");
        t.linear_priority = Some(2);
        t.labels = vec!["model/fable".into(), "bug".into()];
        assert!(rules.warnings[0].message.contains("repeats line 8"), "{:?}", rules.warnings);
        let e = eval(&rules, &t);
        assert_eq!(e.criticality, Criticality::High);
        assert_eq!(e.models, vec![ModelTier::fable()]);
        assert_eq!(e.model_source.as_deref(), Some("if label: model/fable"));
        assert!(e.reasons.contains(&"model fable from ## Models line 8 (if label: model/fable)".to_string()), "{:?}", e.reasons);

        t.labels = vec!["model/sonnet".into()];
        let e = eval(&rules, &t);
        assert_eq!(e.models, vec![ModelTier::sonnet()]);
        assert_eq!(e.model_source.as_deref(), Some("if label ~ ^model/"));

        t.labels = vec!["fable".into()];
        let e = eval(&rules, &t);
        assert_eq!(e.models, vec![ModelTier::opus()], "a loose `fable` label is not `model/fable`");
        assert_eq!(e.model_source.as_deref(), Some("high row"));
        assert!(e.reasons.contains(&"model opus from ## Models".to_string()), "{:?}", e.reasons);

        let mut t9 = linear_task("ENG-9");
        t9.labels = vec!["model/fable".into()];
        let e = eval(&rules, &t9);
        assert_eq!(e.models, vec![ModelTier::haiku()], "## Overrides beats conditional rows");
        assert_eq!(e.model_source.as_deref(), Some("## Overrides"));

        let e = eval(&parse_ok("## High\n- priority: high\n"), &t);
        assert!(e.models.is_empty());
        assert_eq!(e.model_source, None);
    }

    #[test]
    fn qualified_labels_in_conditions() {
        let mut t = linear_task("ENG-1");
        t.labels = vec!["model/fable".into()];
        let m = |c: &Condition, t: &Task| condition_matches(c, t);
        let qualified = Condition::Equals { field: "label".into(), value: "Model/Fable".into() };
        let bare = Condition::Equals { field: "label".into(), value: "fable".into() };
        assert!(m(&qualified, &t));
        assert!(m(&bare, &t), "unqualified rules keep matching child labels");
        assert!(m(&Condition::Matches { field: "label".into(), pattern: "^model/fable$".into() }, &t));
        assert!(
            m(&Condition::Matches { field: "label".into(), pattern: "^fable$".into() }, &t),
            "anchored regexes see the child name"
        );
        assert!(m(&Condition::Matches { field: "label".into(), pattern: "^model/".into() }, &t));
        t.labels = vec!["fable".into()];
        assert!(!m(&qualified, &t));
        assert!(m(&bare, &t));
        assert!(m(&Condition::NotEquals { field: "label".into(), value: "model/fable".into() }, &t));
    }

    #[test]
    fn model_list_errors_and_duplicates() {
        let errs =
            parse_err("## Models\n- critical: fable | llama\n- high: opus |\n## Overrides\n- ENG-1: model = fable | turbo\n");
        let lines: Vec<usize> = errs.iter().map(|e| e.line).collect();
        assert_eq!(lines, vec![2, 3, 5]);
        assert!(errs[0].message.contains("## Models: unknown model `llama`"), "{}", errs[0].message);
        assert!(errs[0].message.contains("fable|opus|sonnet|haiku"), "alias rules in the error: {}", errs[0].message);
        assert!(errs[1].message.contains("empty model name"), "{}", errs[1].message);
        assert!(errs[2].message.starts_with("ENG-1: model: unknown model `turbo`"), "{}", errs[2].message);
        let rules = parse_ok("## Models\n- critical: fable | opus | fable\n");
        assert_eq!(rules.models[&Criticality::Critical], vec![ModelTier::fable(), ModelTier::opus()]);
        assert_eq!(rules.warnings.len(), 1);
        assert!(rules.warnings[0].message.contains("`fable` listed twice"), "{}", rules.warnings[0].message);
    }

    #[test]
    fn jev_section_parses() {
        let rules = parse_ok("## Jev\n- enabled: true\n- question: How urgent?\n- levels: a | b | c\n- colour: blue\n");
        assert!(rules.jev.enabled);
        assert_eq!(rules.jev.question, "How urgent?");
        assert_eq!(rules.jev.levels, vec!["a", "b", "c"]);
        assert_eq!(rules.warnings.len(), 1);
        assert!(rules.warnings[0].message.contains("colour"));
        let errs = parse_err("## Jev\n- levels: only one\n- enabled: maybe\n");
        assert_eq!(errs.iter().map(|e| e.line).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn jev_and_age_contribute_to_score() {
        let rules = parse_ok("");
        let t = linear_task("ENG-1");
        let now = t.created_at + Duration::minutes(366);
        let e = rules.evaluate(&t, now, Some(0.71), 300.0, 2.0);
        let expected = 100.0 + 0.71 * 300.0 + 6.1 * 2.0;
        assert!((e.score - expected).abs() < 1e-6, "{} vs {expected}", e.score);
        assert!(e.reasons.contains(&"jev 0.71 × 300 = +213".to_string()), "{:?}", e.reasons);
        assert!(e.reasons.contains(&"age +12.2 (6.1h)".to_string()), "{:?}", e.reasons);
        // Age boost is capped.
        let old = rules.evaluate(&t, t.created_at + Duration::days(30), None, 300.0, 2.0);
        assert_eq!(old.score, 100.0 + MAX_AGE_BOOST);
        // No Jev score and no age boost configured: base only.
        let plain = rules.evaluate(&t, now, None, 300.0, 0.0);
        assert_eq!(plain.score, 100.0);
        // Jev normalised value is clamped.
        let clamped = rules.evaluate(&t, t.created_at, Some(7.0), 300.0, 0.0);
        assert_eq!(clamped.score, 400.0);
    }

    #[test]
    fn comments_prose_and_unknown_sections_are_tolerated() {
        let md = "# Title\n\nSome prose about rules.\n<!-- a comment\nspanning lines -->\n## Critical\n- label: incident <!-- inline -->\n\n## Notes\n- not a rule at all\n\n* label: oops\n";
        let rules = parse_ok(md);
        assert_eq!(rules.sections[&Criticality::Critical].len(), 1);
        assert_eq!(
            rules.sections[&Criticality::Critical][0].conditions[0],
            Condition::Equals { field: "label".into(), value: "incident".into() }
        );
        assert_eq!(rules.warnings.len(), 1);
        assert_eq!(rules.warnings[0].line, 9);
        assert!(rules.warnings[0].message.contains("Notes"));
    }

    #[test]
    fn bullet_before_section_warns() {
        let rules = parse_ok("- label: x\n## Low\n- label: y\n");
        assert_eq!(rules.warnings.len(), 1);
        assert_eq!(rules.warnings[0].line, 1);
    }

    #[test]
    fn errors_carry_line_numbers() {
        let errs = parse_err(
            "## Critical\n- label: ok\n- bogus\n- colour: red\n- title > 3\n- estimate > many\n- title ~ (unclosed\n- criticality: high\n- source: email\n",
        );
        let lines: Vec<usize> = errs.iter().map(|e| e.line).collect();
        assert_eq!(lines, vec![3, 4, 5, 6, 7, 8, 9]);
        assert!(errs[0].message.contains("cannot parse condition"));
        assert!(errs[1].message.contains("unknown field `colour`"));
        assert!(errs[2].message.contains("numeric field"));
        assert!(errs[3].message.contains("needs a number"));
        assert!(errs[4].message.contains("invalid regex"));
        assert!(errs[5].message.contains("criticality"));
        assert!(errs[6].message.contains("source"));
        assert_eq!(errs[0].to_string(), format!("PRIORITY.md line 3: {}", errs[0].message));
    }

    #[test]
    fn scoring_and_override_errors() {
        let errs = parse_err(
            "## Scoring\n- 40 if label: x\n- +40\n- +40 if\n## Overrides\n- ENG-1: purple\n- ENG-2 skip\n- ENG-3: model = gpt\n## Models\n- critical: gpt\n- weird: opus\n",
        );
        let lines: Vec<usize> = errs.iter().map(|e| e.line).collect();
        assert_eq!(lines, vec![2, 3, 4, 6, 7, 8, 10, 11]);
    }

    #[test]
    fn priority_names_and_numbers() {
        let rules =
            parse_ok("## Critical\n- priority: urgent\n## High\n- priority < 3 and priority != none\n## Low\n- priority: none\n");
        let mut t = linear_task("ENG-1");
        t.linear_priority = Some(1);
        assert_eq!(eval(&rules, &t).criticality, Criticality::Critical);
        t.linear_priority = Some(2);
        assert_eq!(eval(&rules, &t).criticality, Criticality::High);
        t.linear_priority = Some(0);
        assert_eq!(eval(&rules, &t).criticality, Criticality::Low);
        t.linear_priority = None;
        assert_eq!(eval(&rules, &t).criticality, Criticality::Normal);
        assert!(parse_err("## High\n- priority: 9\n")[0].message.contains("priority"));
    }

    #[test]
    fn multiple_conditions_must_all_match() {
        let rules = parse_ok("## Critical\n- priority: urgent AND label: customer\n");
        let mut t = linear_task("ENG-1");
        t.linear_priority = Some(1);
        assert_eq!(eval(&rules, &t).criticality, Criticality::Normal);
        t.labels = vec!["customer".into()];
        assert_eq!(eval(&rules, &t).criticality, Criticality::Critical);
    }

    #[test]
    fn describe_is_readable() {
        let rules = parse_ok(crate::priority::template());
        let text = rules.describe();
        assert!(text.contains("Critical (3 rules)"));
        assert!(text.contains("label: incident"));
        assert!(text.contains("Default: normal"));
        assert!(text.contains("+40 if label: customer"));
        assert!(text.contains("critical: fable"));
        assert!(text.contains("Jev: disabled"));
    }

    #[test]
    fn cycle_fields_match_like_project_and_estimate() {
        let rules = parse_ok(
            "## High\n- cycle: active\n## Low\n- cycle: future\n\
             ## Scoring\n- +150 if cycle: active\n- -100 if cycle: future\n- +5 if cycle_number > 10\n- +1 if cycle_number: 12\n- +7 if cycle != active\n",
        );
        let mut t = linear_task("ENG-1");
        t.cycle = Some("active".into());
        t.cycle_number = Some(12);
        let e = eval(&rules, &t);
        assert_eq!(e.criticality, Criticality::High);
        assert_eq!(e.score, Criticality::High.base_score() + 150.0 + 5.0 + 1.0, "{:?}", e.reasons);

        t.cycle = Some("future".into());
        t.cycle_number = Some(3);
        let e = eval(&rules, &t);
        assert_eq!(e.criticality, Criticality::Low);
        assert_eq!(e.score, Criticality::Low.base_score() - 100.0 + 7.0, "{:?}", e.reasons);

        // No cycle: nothing matches except `!=`.
        t.cycle = None;
        t.cycle_number = None;
        let e = eval(&rules, &t);
        assert_eq!(e.criticality, Criticality::Normal);
        assert_eq!(e.score, Criticality::Normal.base_score() + 7.0, "{:?}", e.reasons);
        assert!(!condition_matches(&Condition::Matches { field: "cycle".into(), pattern: "act".into() }, &linear_task("x")));

        // Values are validated at parse time.
        let errs = parse_err("## High\n- cycle: sprint-3\n- cycle_number > soon\n");
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs[0].message.contains("active|next|past|future"), "{}", errs[0].message);
        assert!(errs[1].message.contains("needs a number"), "{}", errs[1].message);
        assert!(parse_err("## High\n- cycle > 3\n")[0].message.contains("numeric field"));
    }

    #[test]
    fn load_reports_missing_file_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("PRIORITY.md");
        let err = PriorityRules::load(&path).unwrap_err();
        assert!(err.to_string().contains("cannot read"));
        std::fs::write(&path, "## High\n- nonsense\n").unwrap();
        let err = PriorityRules::load(&path).unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");
        std::fs::write(&path, crate::priority::template()).unwrap();
        assert!(PriorityRules::load(&path).is_ok());
    }

    #[test]
    fn condition_display_round_trips_through_parser() {
        let rules = parse_ok("## High\n- label: a and title ~ b and estimate > 1 and priority < 4 and project != X\n");
        let text = fmt_conditions(&rules.sections[&Criticality::High][0].conditions);
        assert_eq!(text, "label: a and title ~ b and estimate > 1 and priority < 4 and project != X");
        let again = parse_ok(&format!("## High\n- {text}\n"));
        assert_eq!(again.sections, rules.sections);
    }
}
