//! The decision: which tier may a task use *now*?
//!
//! Rules, in order:
//! 1. Hard overrides (`task.model_override`, PRIORITY.md `## Models`) pick a
//!    preferred tier; the policy may still downgrade when that tier is out of
//!    budget, and says so in `reasons`.
//! 2. A tier that reported `rate_limit` is unavailable until its cooldown ends.
//! 3. The rolling window must have room for the predicted cost.
//! 4. A tier is *eligible* for a task when `task.criticality <= min_criticality`,
//!    or when it is relaxed: after `relax_after_fraction` of the period, if the
//!    tier's spend fraction is below the period's elapsed fraction (it is
//!    under-paced), one criticality level lower qualifies; in the end game
//!    (`endgame_fraction`) two levels lower qualify as long as the predicted
//!    cost fits the tier's remaining budget with the safety margin.
//! 5. Among eligible tiers prefer the preferred tier, else the most capable.
//! 6. If nothing is eligible, the task is throttled until the earlier of the
//!    window roll-over and the period end.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::BudgetConfig;
use crate::domain::{ModelTier, Task};

use super::estimator::Prediction;
use super::ledger::Ledger;

/// Per-tier cooldown after a rate-limit error, persisted in kv `budget.rate_limits`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitState {
    pub exhausted_until: std::collections::BTreeMap<ModelTier, DateTime<Utc>>,
}

impl RateLimitState {
    pub fn is_exhausted(&self, tier: ModelTier, now: DateTime<Utc>) -> bool {
        self.exhausted_until.get(&tier).map(|t| *t > now).unwrap_or(false)
    }
    pub fn mark(&mut self, tier: ModelTier, until: DateTime<Utc>) {
        self.exhausted_until.insert(tier, until);
    }
    pub fn clear_expired(&mut self, now: DateTime<Utc>) {
        self.exhausted_until.retain(|_, t| *t > now);
    }
}

/// Outcome for one task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// `None` = do not run now.
    pub model: Option<ModelTier>,
    /// When `model` is `None`, when to look again.
    pub retry_at: Option<DateTime<Utc>>,
    pub prediction: Prediction,
    /// Trail for `task explain` and the dashboard.
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Policy<'a> {
    pub cfg: &'a BudgetConfig,
    pub ledger: &'a Ledger,
    pub rate_limits: &'a RateLimitState,
}

impl<'a> Policy<'a> {
    pub fn new(cfg: &'a BudgetConfig, ledger: &'a Ledger, rate_limits: &'a RateLimitState) -> Self {
        Self { cfg, ledger, rate_limits }
    }

    /// Decide for `task`, given its predicted cost and a preferred tier.
    pub fn decide(&self, task: &Task, prediction: Prediction, preferred: Option<ModelTier>) -> Decision {
        let _ = (task, prediction, preferred);
        todo!("TODO(agent-budget)")
    }

    /// Is `tier` eligible for this task right now, and why/why not?
    pub fn eligibility(&self, task: &Task, tier: ModelTier, predicted_weighted: f64) -> (bool, String) {
        let _ = (task, tier, predicted_weighted);
        todo!("TODO(agent-budget)")
    }
}
