//! Client for Jev, TypeSafe's "System One" decision model.
//!
//! Jev answers fixed-form questions with probabilities instead of prose, which
//! makes it a good fit for turning a ticket into an ordered urgency level.
//! Endpoint: `POST https://api.typesafe.ai/v1/systemone`, bearer auth.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A `score` question: ordered rubric levels, lowest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevQuestion {
    pub instructions: String,
    /// Level descriptions from least to most important. 2..=10 entries.
    pub levels: Vec<String>,
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

#[derive(Debug, Clone)]
pub struct JevClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
}

impl JevClient {
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(format!("powerqueue/{}", crate::VERSION))
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        Ok(Self { http, endpoint: endpoint.into(), api_key: api_key.into(), model: model.into() })
    }

    /// Ask one `score` question about `state` (free text or JSON).
    pub async fn score(&self, state: &serde_json::Value, question: &JevQuestion) -> Result<JevScore> {
        let _ = (&self.http, &self.endpoint, &self.api_key, &self.model, state, question);
        todo!("TODO(agent-linear): POST body = model, state, questions.priority = score question with criteria = levels")
    }

    /// Cheap connectivity check used by `init`/`doctor`.
    pub async fn ping(&self) -> Result<()> {
        todo!("TODO(agent-linear)")
    }
}

/// Stable hash of the content Jev scored, to decide when to re-score.
pub fn content_hash(title: &str, description: &str, labels: &[String]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    title.hash(&mut h);
    description.hash(&mut h);
    labels.hash(&mut h);
    format!("{:016x}", h.finish())
}
