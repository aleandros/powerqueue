//! Learns how expensive tasks are from completed history.
//!
//! Prediction looks for the most specific bucket of past tasks that resembles
//! the new one (same estimate → same criticality → same first label → all)
//! and uses that bucket's median, shrunk toward the global median when the
//! bucket is thin. Failed attempts count 1.5× because they burned budget
//! without producing a result, which is the pessimistic view a scheduler wants.

use serde::{Deserialize, Serialize};

use crate::domain::{Criticality, Task, TaskState};
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

impl Prediction {
    /// The prediction used when there is no history at all.
    pub fn default_guess() -> Self {
        Self {
            weighted_tokens: Estimator::DEFAULT_WEIGHTED_TOKENS,
            wall_secs: Estimator::DEFAULT_WALL_SECS,
            confidence: 0.0,
            basis: "default (no history)".to_string(),
        }
    }
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

impl Sample {
    /// Cost as the scheduler should count it: failed attempts are penalised.
    fn effective_tokens(&self) -> f64 {
        if self.completed { self.weighted_tokens } else { self.weighted_tokens * Estimator::FAILED_PENALTY }
    }
    fn effective_wall(&self) -> f64 {
        if self.completed { self.wall_secs } else { self.wall_secs * Estimator::FAILED_PENALTY }
    }
    fn first_label(&self) -> Option<&str> {
        self.labels.first().map(String::as_str)
    }
}

/// What a prediction is keyed on; extracted from a [`Task`] or a [`Sample`].
#[derive(Debug, Clone, Copy)]
struct Features<'a> {
    criticality: Criticality,
    estimate: Option<f64>,
    first_label: Option<&'a str>,
}

impl Estimator {
    /// Default guess when there is no history (≈ one focused Sonnet session).
    pub const DEFAULT_WEIGHTED_TOKENS: f64 = 1_500_000.0;
    pub const DEFAULT_WALL_SECS: f64 = 1_800.0;
    /// Multiplier applied to failed attempts (they wasted budget).
    pub const FAILED_PENALTY: f64 = 1.5;
    /// Shrinkage strength: pseudo-count of global samples mixed into thin buckets.
    pub const SHRINK_K: f64 = 3.0;
    /// Buckets at least this large are trusted without shrinkage.
    pub const TRUST_N: usize = 3;
    /// Samples needed for full confidence.
    const FULL_CONFIDENCE_N: f64 = 10.0;

    /// Build from per-task summaries, keeping completed and failed tasks that
    /// actually consumed tokens. Running or cancelled tasks are not evidence.
    pub fn from_summaries(summaries: &[TaskUsageSummary]) -> Self {
        let samples = summaries
            .iter()
            .filter(|s| s.weighted > 0.0 && matches!(s.state, TaskState::Completed | TaskState::Failed))
            .map(|s| Sample {
                criticality: s.criticality,
                estimate: s.estimate,
                labels: s.labels.clone(),
                weighted_tokens: s.weighted,
                wall_secs: s.wall_secs.max(0) as f64,
                completed: s.state == TaskState::Completed,
            })
            .collect();
        Self { samples }
    }

    /// Predict the cost of `task` from the most specific matching bucket.
    pub fn predict(&self, task: &Task) -> Prediction {
        self.predict_features(
            Features {
                criticality: task.criticality,
                estimate: task.estimate,
                first_label: task.labels.first().map(String::as_str),
            },
            None,
        )
    }

    /// Mean absolute percentage error of leave-one-out predictions over
    /// completed samples, as a fraction (0.25 = predictions are off by 25% on
    /// average). `None` with fewer than four completed samples.
    pub fn accuracy(&self) -> Option<f64> {
        let completed: Vec<usize> = (0..self.samples.len()).filter(|&i| self.samples[i].completed).collect();
        if completed.len() < 4 {
            return None;
        }
        let mut total = 0.0;
        let mut n = 0usize;
        for &i in &completed {
            let s = &self.samples[i];
            if s.weighted_tokens <= 0.0 {
                continue;
            }
            let pred = self.predict_features(
                Features { criticality: s.criticality, estimate: s.estimate, first_label: s.first_label() },
                Some(i),
            );
            total += (pred.weighted_tokens - s.weighted_tokens).abs() / s.weighted_tokens;
            n += 1;
        }
        if n == 0 { None } else { Some(total / n as f64) }
    }

    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Core prediction. `exclude` leaves one sample out (for [`Self::accuracy`]).
    fn predict_features(&self, f: Features<'_>, exclude: Option<usize>) -> Prediction {
        let all: Vec<&Sample> = self.samples.iter().enumerate().filter(|(i, _)| Some(*i) != exclude).map(|(_, s)| s).collect();
        if all.is_empty() {
            return Prediction::default_guess();
        }
        let global_tokens = median(all.iter().map(|s| s.effective_tokens()));
        let global_wall = median(all.iter().map(|s| s.effective_wall()));

        let buckets: [(String, Vec<&Sample>); 3] = [
            (
                f.estimate.map(|e| format!("estimate={e}")).unwrap_or_default(),
                match f.estimate {
                    Some(e) => all.iter().copied().filter(|s| s.estimate.is_some_and(|se| (se - e).abs() < 1e-9)).collect(),
                    None => Vec::new(),
                },
            ),
            (format!("criticality={}", f.criticality), all.iter().copied().filter(|s| s.criticality == f.criticality).collect()),
            (
                f.first_label.map(|l| format!("label={l}")).unwrap_or_default(),
                match f.first_label {
                    Some(l) => {
                        all.iter().copied().filter(|s| s.first_label().is_some_and(|sl| sl.eq_ignore_ascii_case(l))).collect()
                    }
                    None => Vec::new(),
                },
            ),
        ];

        let chosen = buckets.iter().find(|(_, members)| !members.is_empty());
        let (name, members) = match chosen {
            Some((name, members)) => (name.as_str(), members.as_slice()),
            None => ("global", all.as_slice()),
        };
        let n = members.len();
        let bucket_tokens = median(members.iter().map(|s| s.effective_tokens()));
        let bucket_wall = median(members.iter().map(|s| s.effective_wall()));
        let (tokens, wall, basis) = if n >= Self::TRUST_N {
            (bucket_tokens, bucket_wall, format!("{name} (n={n})"))
        } else {
            (
                shrink(bucket_tokens, n, global_tokens),
                shrink(bucket_wall, n, global_wall),
                format!("{name} (n={n}, shrunk toward global median of {})", all.len()),
            )
        };
        Prediction { weighted_tokens: tokens, wall_secs: wall, confidence: (n as f64 / Self::FULL_CONFIDENCE_N).min(1.0), basis }
    }
}

/// `(n·bucket + k·global) / (n + k)`.
fn shrink(bucket_median: f64, n: usize, global_median: f64) -> f64 {
    let n = n as f64;
    (n * bucket_median + Estimator::SHRINK_K * global_median) / (n + Estimator::SHRINK_K)
}

/// Median of a non-empty sequence (0 when empty).
fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut v: Vec<f64> = values.filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) { (v[mid - 1] + v[mid]) / 2.0 } else { v[mid] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{TaskId, TaskSource, TokenUsage};

    fn summary(
        state: TaskState,
        crit: Criticality,
        estimate: Option<f64>,
        labels: &[&str],
        weighted: f64,
        wall: i64,
    ) -> TaskUsageSummary {
        TaskUsageSummary {
            task_id: TaskId::new(),
            criticality: crit,
            estimate,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            state,
            attempts: 1,
            usage: TokenUsage::default(),
            weighted,
            wall_secs: wall,
            tier: None,
        }
    }

    fn task(crit: Criticality, estimate: Option<f64>, labels: &[&str]) -> Task {
        let mut t = Task::new("ENG-1", "t", TaskSource::Manual);
        t.criticality = crit;
        t.estimate = estimate;
        t.labels = labels.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn no_history_returns_defaults() {
        let est = Estimator::from_summaries(&[]);
        let p = est.predict(&task(Criticality::Normal, None, &[]));
        assert_eq!(p.weighted_tokens, Estimator::DEFAULT_WEIGHTED_TOKENS);
        assert_eq!(p.wall_secs, Estimator::DEFAULT_WALL_SECS);
        assert_eq!(p.confidence, 0.0);
        assert_eq!(p.basis, "default (no history)");
        assert_eq!(est.accuracy(), None);
        assert_eq!(est.sample_count(), 0);
    }

    #[test]
    fn only_completed_and_failed_with_usage_become_samples() {
        let est = Estimator::from_summaries(&[
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
            summary(TaskState::Failed, Criticality::Normal, None, &[], 200.0, 20),
            summary(TaskState::Running, Criticality::Normal, None, &[], 300.0, 30),
            summary(TaskState::Cancelled, Criticality::Normal, None, &[], 400.0, 40),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 0.0, 50),
        ]);
        assert_eq!(est.sample_count(), 2);
        assert!(est.samples[0].completed);
        assert!(!est.samples[1].completed);
    }

    #[test]
    fn failed_attempts_are_penalised() {
        let est = Estimator::from_summaries(&[
            summary(TaskState::Failed, Criticality::Normal, None, &[], 1000.0, 100),
            summary(TaskState::Failed, Criticality::Normal, None, &[], 1000.0, 100),
            summary(TaskState::Failed, Criticality::Normal, None, &[], 1000.0, 100),
        ]);
        let p = est.predict(&task(Criticality::Normal, None, &[]));
        assert_eq!(p.weighted_tokens, 1500.0);
        assert_eq!(p.wall_secs, 150.0);
    }

    #[test]
    fn estimate_bucket_is_preferred_when_large_enough() {
        let mut rows = vec![
            summary(TaskState::Completed, Criticality::Normal, Some(3.0), &["bug"], 500.0, 100),
            summary(TaskState::Completed, Criticality::Normal, Some(3.0), &["bug"], 600.0, 100),
            summary(TaskState::Completed, Criticality::High, Some(3.0), &["bug"], 700.0, 100),
        ];
        for _ in 0..5 {
            rows.push(summary(TaskState::Completed, Criticality::Normal, Some(8.0), &["feature"], 5000.0, 1000));
        }
        let est = Estimator::from_summaries(&rows);
        let p = est.predict(&task(Criticality::Normal, Some(3.0), &["feature"]));
        assert_eq!(p.weighted_tokens, 600.0, "median of the estimate=3 bucket, not the label bucket");
        assert_eq!(p.basis, "estimate=3 (n=3)");
        assert!((p.confidence - 0.3).abs() < 1e-9);
    }

    #[test]
    fn thin_buckets_are_shrunk_toward_the_global_median() {
        let est = Estimator::from_summaries(&[
            summary(TaskState::Completed, Criticality::Critical, None, &[], 10_000.0, 100),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 1000.0, 100),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 1000.0, 100),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 1000.0, 100),
        ]);
        let p = est.predict(&task(Criticality::Critical, None, &[]));
        // global median = 1000; bucket n=1 median 10000 → (10000 + 3·1000)/4 = 3250
        assert_eq!(p.weighted_tokens, 3250.0);
        assert!(p.basis.starts_with("criticality=critical (n=1, shrunk"), "{}", p.basis);
        assert!((p.confidence - 0.1).abs() < 1e-9);
    }

    #[test]
    fn falls_back_through_criticality_label_then_global() {
        let est = Estimator::from_summaries(&[
            summary(TaskState::Completed, Criticality::High, None, &["infra"], 100.0, 10),
            summary(TaskState::Completed, Criticality::High, None, &["infra"], 100.0, 10),
            summary(TaskState::Completed, Criticality::High, None, &["infra"], 100.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &["INFRA"], 900.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &["infra"], 900.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &["infra"], 900.0, 10),
        ]);
        // estimate unknown, criticality Low has no bucket, label matches 6 → label bucket.
        let p = est.predict(&task(Criticality::Low, None, &["Infra"]));
        assert_eq!(p.basis, "label=Infra (n=6)");
        assert_eq!(p.weighted_tokens, 500.0);
        // nothing matches at all → global.
        let p = est.predict(&task(Criticality::Low, Some(99.0), &["other"]));
        assert_eq!(p.basis, "global (n=6)");
        assert_eq!(p.weighted_tokens, 500.0);
        // criticality bucket wins over label bucket.
        let p = est.predict(&task(Criticality::High, None, &["infra"]));
        assert_eq!(p.basis, "criticality=high (n=3)");
        assert_eq!(p.weighted_tokens, 100.0);
    }

    #[test]
    fn confidence_saturates_at_one() {
        let rows: Vec<_> =
            (0..25).map(|i| summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0 + i as f64, 10)).collect();
        let est = Estimator::from_summaries(&rows);
        let p = est.predict(&task(Criticality::Normal, None, &[]));
        assert_eq!(p.confidence, 1.0);
        assert_eq!(p.weighted_tokens, 112.0);
    }

    #[test]
    fn accuracy_is_leave_one_out_mape() {
        let est = Estimator::from_summaries(&[
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
        ]);
        assert_eq!(est.accuracy(), None, "fewer than four completed samples");
        let est = Estimator::from_summaries(&[
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
        ]);
        assert_eq!(est.accuracy(), Some(0.0), "perfectly regular history predicts itself");
        let est = Estimator::from_summaries(&[
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 100.0, 10),
            summary(TaskState::Completed, Criticality::Normal, None, &[], 200.0, 10),
        ]);
        // Leaving the 200 out predicts 100 → 50% error; others predict 100 → 0%.
        assert!((est.accuracy().unwrap() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn median_handles_even_and_odd_counts() {
        assert_eq!(median([3.0, 1.0, 2.0].into_iter()), 2.0);
        assert_eq!(median([4.0, 1.0, 3.0, 2.0].into_iter()), 2.5);
        assert_eq!(median(std::iter::empty()), 0.0);
        assert_eq!(median([f64::NAN, 5.0].into_iter()), 5.0);
    }
}
