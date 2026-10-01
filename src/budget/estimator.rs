//! Learns how expensive tasks are from completed history.

use serde::{Deserialize, Serialize};

use crate::domain::{Criticality, Task};
use crate::store::TaskUsageSummary;

/// A cost prediction for one task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prediction {
    /// Expected weighted tokens (before tier weight).
    pub weighted_tokens: f64,
    /// Expected wall-clock seconds.
    pub wall_secs: f64,
    /// 0..=1: how much history backs this (0 = global default).
    pub confidence: f64,
    /// Which bucket the prediction came from, e.g. "estimate=3 (n=4)".
    pub basis: String,
}

/// Groups history by estimate points, criticality and labels and predicts
/// with a shrinkage estimate toward the global median.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Estimator {
    pub samples: Vec<Sample>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub criticality: Criticality,
    pub estimate: Option<f64>,
    pub labels: Vec<String>,
    pub weighted_tokens: f64,
    pub wall_secs: f64,
    pub completed: bool,
}

impl Estimator {
    /// Default guess when there is no history (≈ one focused Sonnet session).
    pub const DEFAULT_WEIGHTED_TOKENS: f64 = 1_500_000.0;
    pub const DEFAULT_WALL_SECS: f64 = 1_800.0;

    pub fn from_summaries(summaries: &[TaskUsageSummary]) -> Self {
        let _ = summaries;
        todo!("TODO(agent-budget): keep completed (and failed, flagged) tasks with usage > 0")
    }

    pub fn predict(&self, task: &Task) -> Prediction {
        let _ = task;
        todo!("TODO(agent-budget)")
    }

    /// Mean absolute percentage error of leave-one-out predictions, for `doctor`.
    pub fn accuracy(&self) -> Option<f64> {
        todo!("TODO(agent-budget)")
    }

    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }
}
