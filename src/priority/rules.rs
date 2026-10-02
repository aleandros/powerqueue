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

/// Per-ticket pin under `## Overrides`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Override {
    Criticality(Criticality),
    Score(f64),
    Model(ModelTier),
    /// Never schedule this ticket.
    Skip,
}

impl fmt::Display for Override {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Override::Criticality(c) => write!(f, "{c}"),
            Override::Score(d) => write!(f, "{}", fmt_delta(*d)),
            Override::Model(m) => write!(f, "model = {m}"),
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

/// Maximum score a task can gain from waiting.
pub const MAX_AGE_BOOST: f64 = 200.0;

/// Fields a condition may reference.
const FIELDS: [&str; 9] = ["label", "priority", "estimate", "project", "team", "title", "description", "source", "key"];
/// Fields that support `>` / `<`.
const NUMERIC_FIELDS: [&str; 2] = ["priority", "estimate"];

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
                Section::Overrides => match parse_override(bullet, line_no) {
                    Ok((key, ov)) => rules.overrides.entry(key).or_default().push(ov),
                    Err(e) => errors.push(e),
                },
                Section::Models => match parse_model_line(bullet, line_no) {
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
        let mut model: Option<ModelTier> = None;
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
                        model = Some(m.clone());
                        reasons.push(format!("model {m} from ## Overrides"));
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

        if model.is_none()
            && let Some(m) = self.models.get(&criticality)
        {
            model = Some(m.clone());
            reasons.push(format!("model {m} from ## Models"));
        }

        Evaluation { criticality, score, model, skip, reasons }
    }

    /// Model the rules assign to a criticality, if any.
    pub fn model_for(&self, criticality: Criticality) -> Option<ModelTier> {
        self.models.get(&criticality).cloned()
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
        for c in Criticality::ALL {
            match self.models.get(&c) {
                Some(m) => out.push_str(&format!("  {c}: {m}\n")),
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
/// A missing optional field (`priority`, `estimate`, `project`, `team`) never
/// matches, except for `!=`, which does.
pub fn condition_matches(cond: &Condition, task: &Task) -> bool {
    match cond {
        Condition::Equals { field, value } => field_equals(field, value, task).unwrap_or(false),
        Condition::NotEquals { field, value } => !field_equals(field, value, task).unwrap_or(false),
        Condition::Matches { field, pattern } => {
            let Some(re) = compile_regex(pattern) else { return false };
            if field == "label" {
                return task.labels.iter().any(|l| re.is_match(l));
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
        "label" => Some(task.labels.iter().any(|l| l.trim().eq_ignore_ascii_case(value.trim()))),
        "priority" => {
            let wanted = parse_priority_value(value).ok()?;
            task.linear_priority.map(|p| f64::from(p) == wanted)
        }
        "estimate" => {
            let wanted = value.trim().parse::<f64>().ok()?;
            task.estimate.map(|e| (e - wanted).abs() < 1e-9)
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
        "team" => match &task.source {
            TaskSource::Linear { team_key, .. } => Some(team_key.clone()),
            TaskSource::Manual => None,
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
        "priority" | "estimate" => parse_number_for(field, value, line).map(|_| ()),
        "source" => {
            if matches!(value.to_ascii_lowercase().as_str(), "linear" | "manual") {
                Ok(())
            } else {
                Err(RuleError::new(line, format!("`source` must be `linear` or `manual`, not `{value}`")))
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

fn priority_name(p: u8) -> &'static str {
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

fn parse_override(text: &str, line: usize) -> Result<(String, Override), RuleError> {
    let (key, value) = text
        .split_once(':')
        .ok_or_else(|| RuleError::new(line, format!("cannot parse override `{text}` (expected `KEY: critical|high|normal|low`, `KEY: +N`, `KEY: model = <tier>` or `KEY: skip`)")))?;
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
        let tier = rest.trim_start().strip_prefix(['=', ':']).map(str::trim).unwrap_or("");
        let tier = tier.parse::<ModelTier>().map_err(|e| RuleError::new(line, format!("{key}: {e}")))?;
        Override::Model(tier)
    } else {
        let c = value.parse::<Criticality>().map_err(|_| {
            RuleError::new(line, format!("cannot parse override value `{value}` for {key} (expected critical|high|normal|low, +N, -N, model = <tier> or skip)"))
        })?;
        Override::Criticality(c)
    };
    Ok((key.to_ascii_lowercase(), ov))
}

fn parse_model_line(text: &str, line: usize) -> Result<(Criticality, ModelTier), RuleError> {
    let (c, m) = text
        .split_once(':')
        .or_else(|| text.split_once('='))
        .ok_or_else(|| RuleError::new(line, format!("cannot parse model line `{text}` (expected `<criticality>: <tier>`)")))?;
    let c = c.trim().parse::<Criticality>().map_err(|e| RuleError::new(line, format!("## Models: {e}")))?;
    let m = m.trim().parse::<ModelTier>().map_err(|e| RuleError::new(line, format!("## Models: {e}")))?;
    Ok((c, m))
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

fn fmt_conditions(conds: &[Condition]) -> String {
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
        assert_eq!(rules.models[&Criticality::Critical], ModelTier::fable());
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
        assert_eq!(rules.model_for(Criticality::Low), None);
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
