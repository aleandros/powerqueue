//! Client for Jev, TypeSafe's "System One" decision model.
//!
//! Jev answers fixed-form questions with probabilities instead of prose, which
//! makes it a good fit for turning a ticket into an ordered urgency level.
//! Endpoint: `POST https://api.typesafe.ai/v1/systemone`, bearer auth.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::domain::Task;
use crate::priority::JevSection;

/// A `score` question: ordered rubric levels, lowest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevQuestion {
    pub instructions: String,
    /// Level descriptions from least to most important. 2..=10 entries.
    pub levels: Vec<String>,
}

impl JevQuestion {
    /// Build the question from the `## Jev` section of `PRIORITY.md`.
    pub fn from_section(section: &JevSection) -> Self {
        Self { instructions: section.question.clone(), levels: section.levels.clone() }
    }
}

/// Result of scoring one piece of state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevScore {
    /// Probability-weighted mean of level indices (0 ..= levels-1).
    pub score: f64,
    /// Index of the most probable level.
    pub level_index: usize,
    /// Description of that level.
    pub level: String,
    pub confidence: f64,
    pub probabilities: Vec<f64>,
    pub input_tokens: u64,
    /// Full response for diagnostics.
    pub raw: serde_json::Value,
}

impl JevScore {
    /// Score normalised to 0..=1 across the rubric.
    pub fn normalized(&self) -> f64 {
        let n = self.probabilities.len().max(2) as f64;
        (self.score / (n - 1.0)).clamp(0.0, 1.0)
    }
}

/// Name of the single question powerqueue asks per request.
const QUESTION_KEY: &str = "priority";
/// Descriptions longer than this are cut before being sent (tokens cost money).
const MAX_DESCRIPTION_CHARS: usize = 4000;

/// Raw response shape; unknown fields are ignored, `legend`/`probabilities`
/// may be keyed by string indices.
#[derive(Debug, Deserialize)]
struct RawResponse {
    #[serde(default)]
    answers: BTreeMap<String, RawAnswer>,
    #[serde(default)]
    usage: Option<RawUsage>,
}

#[derive(Debug, Deserialize)]
struct RawAnswer {
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    legend: Option<IndexMap<String>>,
    #[serde(default)]
    probabilities: Option<IndexMap<f64>>,
    #[serde(default)]
    confidence: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
}

/// Either an array or an object keyed by stringified indices.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IndexMap<T> {
    List(Vec<T>),
    Map(BTreeMap<String, T>),
}

impl<T> IndexMap<T> {
    /// Values ordered by numeric index; non-numeric keys are dropped.
    fn into_ordered(self) -> Vec<T> {
        match self {
            IndexMap::List(v) => v,
            IndexMap::Map(m) => {
                let mut entries: Vec<(usize, T)> =
                    m.into_iter().filter_map(|(k, v)| k.trim().parse::<usize>().ok().map(|i| (i, v))).collect();
                entries.sort_by_key(|(i, _)| *i);
                entries.into_iter().map(|(_, v)| v).collect()
            }
        }
    }
}

/// HTTP client for the scoring endpoint. Cheap to clone.
#[derive(Debug, Clone)]
pub struct JevClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
}

impl JevClient {
    /// Create a client. Fails only if the HTTP client cannot be built.
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(format!("powerqueue/{}", crate::VERSION))
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        Ok(Self { http, endpoint: endpoint.into(), api_key: api_key.into(), model: model.into() })
    }

    /// The endpoint this client posts to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Ask one `score` question about `state` (free text or JSON).
    /// A 401 is reported as an invalid key; other HTTP failures include the
    /// status and an excerpt of the body.
    pub async fn score(&self, state: &serde_json::Value, question: &JevQuestion) -> Result<JevScore> {
        if !(2..=10).contains(&question.levels.len()) {
            bail!("Jev questions need 2 to 10 levels, got {}", question.levels.len());
        }
        let body = serde_json::json!({
            "model": self.model,
            "state": state,
            "questions": {
                QUESTION_KEY: {
                    "type": "score",
                    "instructions": question.instructions,
                    "criteria": question.levels,
                }
            }
        });
        debug!(target: "powerqueue::jev", endpoint = %self.endpoint, model = %self.model, levels = question.levels.len(), "scoring request");
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {}", self.endpoint))?;
        let status = response.status();
        let text = response.text().await.context("read Jev response")?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("Jev rejected the API key (HTTP 401): invalid or expired key");
        }
        if !status.is_success() {
            bail!("Jev returned HTTP {status}: {}", excerpt(&text));
        }
        let raw: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("Jev returned invalid JSON: {}", excerpt(&text)))?;
        parse_score(raw, &question.levels)
    }

    /// Cheap connectivity check used by `init`/`doctor`: a two-level question
    /// on a trivial state. Returns a clear error for an invalid key.
    pub async fn ping(&self) -> Result<()> {
        let question =
            JevQuestion { instructions: "Is this a connectivity check?".into(), levels: vec!["no".into(), "yes".into()] };
        self.score(&serde_json::json!({ "state": "ping" }), &question).await.map(|_| ())
    }

    /// Score a task with the question configured in `## Jev`. The state sent
    /// to Jev is the title, description (cut to 4000 characters), labels,
    /// Linear priority, estimate and project.
    pub async fn score_task(&self, task: &Task, section: &JevSection) -> Result<JevScore> {
        let state = task_state(task);
        let question = JevQuestion::from_section(section);
        let score = self.score(&state, &question).await.with_context(|| format!("Jev scoring of {}", task.key))?;
        debug!(target: "powerqueue::jev", task = %task.key, score = score.score, level = %score.level, confidence = score.confidence, "task scored");
        Ok(score)
    }
}

/// JSON state describing a task for Jev.
pub fn task_state(task: &Task) -> serde_json::Value {
    let description: String = task.description.chars().take(MAX_DESCRIPTION_CHARS).collect();
    let priority = task.linear_priority.map(|p| match p {
        0 => "none",
        1 => "urgent",
        2 => "high",
        3 => "normal",
        4 => "low",
        _ => "unknown",
    });
    serde_json::json!({
        "title": task.title,
        "description": description,
        "labels": task.labels,
        "priority": priority,
        "estimate": task.estimate,
        "project": task.project,
    })
}

/// Turn a raw response into a [`JevScore`], tolerating string-indexed maps
/// and unknown fields.
fn parse_score(raw: serde_json::Value, levels: &[String]) -> Result<JevScore> {
    let mut parsed: RawResponse = serde_json::from_value(raw.clone()).context("parse Jev response")?;
    let answer = match parsed.answers.remove(QUESTION_KEY) {
        Some(a) => a,
        None => parsed
            .answers
            .into_values()
            .next()
            .ok_or_else(|| anyhow!("Jev response has no answers: {}", excerpt(&raw.to_string())))?,
    };
    let probabilities = answer.probabilities.map(IndexMap::into_ordered).unwrap_or_default();
    let legend = answer.legend.map(IndexMap::into_ordered).unwrap_or_default();
    let score = match answer.score {
        Some(s) => s,
        None if !probabilities.is_empty() => probabilities.iter().enumerate().map(|(i, p)| i as f64 * p).sum(),
        None => bail!("Jev answer has neither `score` nor `probabilities`"),
    };
    let level_index = if probabilities.is_empty() {
        score.round().max(0.0) as usize
    } else {
        probabilities
            .iter()
            .enumerate()
            .fold((0usize, f64::NEG_INFINITY), |best, (i, p)| if *p > best.1 { (i, *p) } else { best })
            .0
    };
    let level = legend
        .get(level_index)
        .cloned()
        .or_else(|| levels.get(level_index).cloned())
        .unwrap_or_else(|| format!("level {level_index}"));
    let confidence = answer.confidence.unwrap_or_else(|| probabilities.iter().cloned().fold(0.0, f64::max));
    Ok(JevScore {
        score,
        level_index,
        level,
        confidence,
        probabilities,
        input_tokens: parsed.usage.map(|u| u.input_tokens).unwrap_or(0),
        raw,
    })
}

fn excerpt(text: &str) -> String {
    const MAX: usize = 300;
    let t = text.trim();
    if t.chars().count() > MAX { format!("{}…", t.chars().take(MAX).collect::<String>()) } else { t.to_string() }
}

/// Stable hash of the content Jev scored, to decide when to re-score.
pub fn content_hash(title: &str, description: &str, labels: &[String]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    title.hash(&mut h);
    description.hash(&mut h);
    // Bare child names (`fable`, not `model/fable`), so qualifying grouped
    // labels did not invalidate every cached score at once. Hashes the same
    // as the unqualified `Vec<String>` did.
    let bare: Vec<&str> = labels
        .iter()
        .map(|l| l.rsplit_once(crate::domain::LABEL_PARENT_SEPARATOR).map_or(l.as_str(), |(_, child)| child))
        .collect();
    bare.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskSource;

    fn sample() -> serde_json::Value {
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": { "priority": { "type": "score", "score": 1.43,
                "legend": { "0": "can wait", "1": "nice to have", "2": "important", "3": "blocking" },
                "probabilities": { "0": 0.0, "1": 0.57, "2": 0.43, "3": 0.0 }, "confidence": 0.35 } },
            "usage": { "input_tokens": 210, "output_tokens": 31 },
            "extra": { "ignored": true }
        })
    }

    #[test]
    fn parses_sample_response() {
        let levels: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let s = parse_score(sample(), &levels).unwrap();
        assert_eq!(s.score, 1.43);
        assert_eq!(s.level_index, 1);
        assert_eq!(s.level, "nice to have");
        assert_eq!(s.confidence, 0.35);
        assert_eq!(s.probabilities, vec![0.0, 0.57, 0.43, 0.0]);
        assert_eq!(s.input_tokens, 210);
        assert!((s.normalized() - 1.43 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn tolerates_arrays_and_missing_fields() {
        let raw = serde_json::json!({ "answers": { "priority": { "probabilities": [0.1, 0.9] } } });
        let s = parse_score(raw, &["lo".into(), "hi".into()]).unwrap();
        assert!((s.score - 0.9).abs() < 1e-9);
        assert_eq!(s.level_index, 1);
        assert_eq!(s.level, "hi");
        assert_eq!(s.confidence, 0.9);
        assert_eq!(s.input_tokens, 0);
        let err = parse_score(serde_json::json!({ "answers": {} }), &[]).unwrap_err();
        assert!(err.to_string().contains("no answers"));
    }

    #[test]
    fn task_state_truncates_description() {
        let mut t = Task::new("ENG-1", "Title", TaskSource::Manual);
        t.description = "x".repeat(10_000);
        t.labels = vec!["bug".into()];
        t.linear_priority = Some(1);
        let state = task_state(&t);
        assert_eq!(state["description"].as_str().unwrap().len(), MAX_DESCRIPTION_CHARS);
        assert_eq!(state["priority"], "urgent");
        assert_eq!(state["labels"][0], "bug");
        assert!(state["project"].is_null());
    }

    #[test]
    fn content_hash_is_stable() {
        let a = content_hash("t", "d", &["x".into()]);
        assert_eq!(a, content_hash("t", "d", &["x".into()]));
        assert_ne!(a, content_hash("t", "d2", &["x".into()]));
        assert_eq!(a, content_hash("t", "d", &["group/x".into()]), "qualifying a label keeps the cached score");
    }
}
