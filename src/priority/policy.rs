//! Versioned, strict TOML schema compiled into the shared evaluation representation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use toml::Spanned;

use super::rules::{self, Condition, JevSection, ModelRule, Override, PriorityRules, Rule, RuleError, ScoringRule};
use crate::domain::{Criticality, ModelTier};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    version: u32,
    #[serde(default = "normal")]
    default_criticality: Criticality,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    criticality: Vec<Spanned<CriticalityRule>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scoring: Vec<Spanned<ScoreRule>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    model_rules: Vec<Spanned<PreferenceRule>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    models: BTreeMap<Criticality, Spanned<Vec<ModelTier>>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    overrides: Vec<Spanned<TaskOverride>>,
    #[serde(default)]
    jev: JevSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CriticalityRule {
    level: Criticality,
    all: Vec<Predicate>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoreRule {
    delta: f64,
    all: Vec<Predicate>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreferenceRule {
    models: Vec<ModelTier>,
    all: Vec<Predicate>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskOverride {
    key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    criticality: Option<Criticality>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    score_delta: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    models: Option<Vec<ModelTier>>,
    #[serde(default)]
    skip: bool,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operator {
    Eq,
    Ne,
    Matches,
    Gt,
    Lt,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum Value {
    Text(String),
    Number(f64),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Predicate {
    field: String,
    op: Operator,
    value: Value,
}

fn line(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())].iter().filter(|c| **c == b'\n').count() + 1
}

fn predicates(all: &[Predicate]) -> Result<Vec<Condition>, String> {
    if all.is_empty() {
        return Err("all must contain at least one condition".into());
    }
    all.iter()
        .map(|p| {
            let field = p.field.clone();
            if !rules::FIELDS.contains(&field.as_str()) {
                return Err(format!("unknown field `{field}`; expected {}", rules::FIELDS.join(", ")));
            }
            let value = match &p.value {
                Value::Text(v) => v.clone(),
                Value::Number(v) if v.is_finite() => v.to_string(),
                Value::Number(_) => return Err("condition value must be finite".into()),
            };
            if value.trim().is_empty() {
                return Err("condition value must not be empty".into());
            }
            match p.op {
                Operator::Eq | Operator::Ne => {
                    rules::validate_value(&field, &value, 1).map_err(|e| e.message)?;
                    if rules::NUMERIC_FIELDS.contains(&field.as_str()) && value.parse::<f64>().is_ok_and(|n| !n.is_finite()) {
                        return Err("condition value must be finite".into());
                    }
                    Ok(if matches!(p.op, Operator::Eq) {
                        Condition::Equals { field, value }
                    } else {
                        Condition::NotEquals { field, value }
                    })
                }
                Operator::Matches => {
                    if !matches!(p.value, Value::Text(_)) {
                        return Err("matches requires a string regex".into());
                    }
                    rules::build_regex(&value).map_err(|e| format!("invalid regex: {e}"))?;
                    Ok(Condition::Matches { field, pattern: value })
                }
                Operator::Gt | Operator::Lt => {
                    if !rules::NUMERIC_FIELDS.contains(&field.as_str()) {
                        return Err(format!("gt/lt require a numeric field, not `{field}`"));
                    }
                    let Value::Number(value) = p.value else {
                        return Err("gt/lt require a TOML number".into());
                    };
                    if field == "priority" && !(0.0..=4.0).contains(&value) {
                        return Err("priority must be between 0 and 4".into());
                    }
                    Ok(if matches!(p.op, Operator::Gt) {
                        Condition::GreaterThan { field, value }
                    } else {
                        Condition::LessThan { field, value }
                    })
                }
            }
        })
        .collect()
}

fn models(list: &[ModelTier]) -> Result<(), String> {
    if list.is_empty() {
        return Err("models must contain at least one preference".into());
    }
    for (i, model) in list.iter().enumerate() {
        // ModelTier's general-purpose deserializer normalizes without validating.
        // Policy input must enforce the same alias/provider rules as the CLI.
        model.as_str().parse::<ModelTier>()?;
        if list[..i].contains(model) {
            return Err(format!("duplicate model `{model}`"));
        }
    }
    Ok(())
}
fn finite(value: f64) -> Result<(), String> {
    if value.is_finite() { Ok(()) } else { Err("score delta must be finite".into()) }
}

impl PriorityRules {
    /// Parse a version 1 TOML policy. Unknown keys, invalid predicates and
    /// ambiguous overrides are errors with source lines and schema paths.
    /// Markdown is accepted only by the explicit migration API.
    pub fn parse(text: &str) -> Result<Self, Vec<RuleError>> {
        let policy: Policy = toml::from_str(text).map_err(|e: toml::de::Error| {
            if text.lines().any(|l| l.trim_start().starts_with("## ")) {
                vec![RuleError::new(1, "Markdown rules are retired; run `powerqueue priority migrate --from PRIORITY.md --to priority.toml` and update priority.file / priority_file. Markdown prose alone is not executable.")]
            } else {
                vec![RuleError::new(line(text, e.span().map_or(0, |s| s.start)), e.message())]
            }
        })?;
        let mut out = Self { default_criticality: policy.default_criticality, jev: policy.jev.clone(), ..Self::default() };
        let mut errors = Vec::new();
        if policy.version != 1 {
            errors.push(RuleError::new(1, "version must be 1"));
        }
        if policy.jev.question.trim().is_empty()
            || !(2..=10).contains(&policy.jev.levels.len())
            || policy.jev.levels.iter().any(|v| v.trim().is_empty())
        {
            errors.push(RuleError::new(1, "jev requires a nonempty question and 2 to 10 nonempty levels"));
        }
        for (i, row) in policy.criticality.iter().enumerate() {
            let n = line(text, row.span().start);
            match predicates(&row.get_ref().all) {
                Ok(conditions) => out.sections.entry(row.get_ref().level).or_default().push(Rule { line: n, conditions }),
                Err(e) => errors.push(RuleError::new(n, format!("criticality[{i}].all: {e}"))),
            }
        }
        for (i, row) in policy.scoring.iter().enumerate() {
            let n = line(text, row.span().start);
            let r = row.get_ref();
            match finite(r.delta).and_then(|()| predicates(&r.all)) {
                Ok(conditions) => out.scoring.push(ScoringRule { line: n, delta: r.delta, conditions }),
                Err(e) => errors.push(RuleError::new(n, format!("scoring[{i}]: {e}"))),
            }
        }
        for (i, row) in policy.model_rules.iter().enumerate() {
            let n = line(text, row.span().start);
            let r = row.get_ref();
            match models(&r.models).and_then(|()| predicates(&r.all)) {
                Ok(conditions) => out.model_rules.push(ModelRule { line: n, models: r.models.clone(), conditions }),
                Err(e) => errors.push(RuleError::new(n, format!("model_rules[{i}]: {e}"))),
            }
        }
        for (c, list) in policy.models {
            match models(list.get_ref()) {
                Ok(()) => {
                    out.models.insert(c, list.into_inner());
                }
                Err(e) => errors.push(RuleError::new(line(text, list.span().start), format!("models.{c}: {e}"))),
            }
        }
        for (i, row) in policy.overrides.iter().enumerate() {
            let n = line(text, row.span().start);
            let r = row.get_ref();
            let result = (|| {
                let key = r.key.trim().to_ascii_lowercase();
                if key.is_empty() || key.contains(char::is_whitespace) {
                    return Err("key must be a single task key".into());
                }
                if out.overrides.contains_key(&key) {
                    return Err(format!("duplicate override key `{key}`; combine its settings in one row"));
                }
                let mut overrides = Vec::new();
                if let Some(c) = r.criticality {
                    overrides.push(Override::Criticality(c));
                }
                if let Some(d) = r.score_delta {
                    finite(d)?;
                    overrides.push(Override::Score(d));
                }
                if let Some(m) = &r.models {
                    models(m)?;
                    overrides.push(Override::Model(m.clone()));
                }
                if r.skip {
                    overrides.push(Override::Skip);
                }
                if overrides.is_empty() {
                    return Err("override must set criticality, score_delta, models or skip = true".into());
                }
                out.overrides.insert(key, overrides);
                Ok(())
            })();
            if let Err(e) = result {
                errors.push(RuleError::new(n, format!("overrides[{i}]: {e}")));
            }
        }
        if errors.is_empty() { Ok(out) } else { Err(errors) }
    }
}

impl From<&Condition> for Predicate {
    fn from(c: &Condition) -> Self {
        let (op, value) = match c {
            Condition::Equals { value, .. } => (Operator::Eq, Value::Text(value.clone())),
            Condition::NotEquals { value, .. } => (Operator::Ne, Value::Text(value.clone())),
            Condition::Matches { pattern, .. } => (Operator::Matches, Value::Text(pattern.clone())),
            Condition::GreaterThan { value, .. } => (Operator::Gt, Value::Number(*value)),
            Condition::LessThan { value, .. } => (Operator::Lt, Value::Number(*value)),
        };
        Self { field: c.field().into(), op, value }
    }
}

pub(super) fn serialize(r: &PriorityRules) -> anyhow::Result<String> {
    let span = |v| Spanned::new(0..0, v);
    let mut policy = Policy {
        version: 1,
        default_criticality: r.default_criticality,
        criticality: r
            .sections
            .iter()
            .flat_map(|(c, rows)| {
                rows.iter().map(|r| {
                    Spanned::new(0..0, CriticalityRule { level: *c, all: r.conditions.iter().map(Predicate::from).collect() })
                })
            })
            .collect(),
        scoring: r
            .scoring
            .iter()
            .map(|r| Spanned::new(0..0, ScoreRule { delta: r.delta, all: r.conditions.iter().map(Predicate::from).collect() }))
            .collect(),
        model_rules: r
            .model_rules
            .iter()
            .map(|r| {
                Spanned::new(
                    0..0,
                    PreferenceRule { models: r.models.clone(), all: r.conditions.iter().map(Predicate::from).collect() },
                )
            })
            .collect(),
        models: r.models.iter().map(|(c, m)| (*c, Spanned::new(0..0, m.clone()))).collect(),
        overrides: Vec::new(),
        jev: r.jev.clone(),
    };
    for (key, rows) in &r.overrides {
        let mut row = TaskOverride { key: key.clone(), criticality: None, score_delta: None, models: None, skip: false };
        for ov in rows {
            match ov {
                Override::Criticality(c) => row.criticality = Some(*c),
                Override::Score(d) => row.score_delta = Some(row.score_delta.unwrap_or(0.0) + d),
                Override::Model(m) => row.models = Some(m.clone()),
                Override::Skip => row.skip = true,
            }
        }
        policy.overrides.push(span(row));
    }
    Ok(toml::to_string_pretty(&policy)?)
}

fn normal() -> Criticality {
    Criticality::Normal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_schema_rejects_unknown_keys_and_invalid_values_at_locations() {
        for (text, message) in [
            ("version = 1\nunknown = true", "unknown field"),
            ("version = 2", "version"),
            ("version = 1\n[[criticality]]\nlevel = 'high'\nall = []", "at least one"),
            ("version = 1\n[[criticality]]\nlevel = 'high'\nall = [{field = 'title', op = 'gt', value = 1}]", "numeric field"),
            (
                "version = 1\n[[criticality]]\nlevel = 'high'\nall = [{field = 'label', op = 'eq', value = 'x', typo = true}]",
                "unknown field",
            ),
            (
                "version = 1\n[[criticality]]\nlevel = 'high'\nall = [{field = 'title', op = 'matches', value = '('}]",
                "invalid regex",
            ),
            ("version = 1\n[[scoring]]\ndelta = nan\nall = [{field = 'source', op = 'eq', value = 'manual'}]", "finite"),
            ("version = 1\n[models]\nnormal = []", "at least one"),
            ("version = 1\n[models]\nnormal = ['llama']", "unknown model"),
            ("version = 1\n[models]\nnormal = ['sonnet', 'sonnet']", "duplicate model"),
            ("version = 1\n[[overrides]]\nkey = 'X'\nskip = true\n[[overrides]]\nkey = 'x'\nskip = true", "duplicate override"),
            ("version = 1\n[jev]\nunknown = true", "unknown field"),
            ("version = 1\n[jev]\nlevels = ['one']", "2 to 10"),
        ] {
            let errors = PriorityRules::parse(text).unwrap_err();
            assert!(errors.iter().any(|e| e.message.contains(message)), "{text}: {errors:?}");
            assert!(errors.iter().all(|e| e.line > 0 && e.line <= text.lines().count()));
        }
        let errors = PriorityRules::parse(
            "version = 1\n\n[[scoring]]\ndelta = inf\nall = [{field = 'source', op = 'eq', value = 'manual'}]",
        )
        .unwrap_err();
        assert_eq!(errors[0].line, 3);
        assert!(errors[0].message.contains("scoring[0]"));
    }

    #[test]
    fn markdown_and_prose_are_never_runtime_policies() {
        for text in ["", "# Priority intent", "Please run incidents first.", "## Critical\n- label: incident\n"] {
            assert!(PriorityRules::parse(text).is_err());
        }
        assert_eq!(PriorityRules::parse("version = 1").unwrap().default_criticality, Criticality::Normal);
    }
}
