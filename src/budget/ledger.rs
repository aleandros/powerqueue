//! Spend so far, per tier, for the current period and window.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::BudgetConfig;
use crate::domain::{ModelTier, TokenUsage};
use crate::store::Store;

use super::period::{Period, PeriodClock};

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct TierLedger {
    pub tier: ModelTier,
    pub period_usage: TokenUsage,
    pub window_usage: TokenUsage,
    /// Weighted tokens spent this period (usage.weighted() × tier weight).
    pub period_weighted: f64,
    pub window_weighted: f64,
    /// Weighted budget for this tier in the period (`share × period_weighted_tokens`).
    pub period_budget: f64,
    pub messages: u64,
}

impl TierLedger {
    pub fn period_spent_fraction(&self) -> f64 {
        if self.period_budget <= 0.0 { 1.0 } else { (self.period_weighted / self.period_budget).max(0.0) }
    }
    pub fn period_remaining(&self) -> f64 {
        (self.period_budget - self.period_weighted).max(0.0)
    }
}

/// Snapshot used by the policy and the dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub now: DateTime<Utc>,
    pub period: Period,
    pub window: Period,
    pub tiers: Vec<TierLedger>,
    pub total_period_weighted: f64,
    pub total_window_weighted: f64,
    pub period_budget: f64,
    pub window_budget: f64,
    /// Observed usage percentage from `/usage`, if the user calibrated recently.
    pub calibration: Option<Calibration>,
}

/// User-reported usage (`powerqueue budget set-observed 43%`) so pacing can
/// correct for usage outside powerqueue (interactive sessions, other tools).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub observed_fraction: f64,
    pub at: DateTime<Utc>,
    /// Our own measured period fraction at the time, to compute an offset.
    pub measured_fraction: f64,
}

impl Ledger {
    /// Aggregate usage rows into a ledger.
    pub fn load(store: &Store, cfg: &BudgetConfig, clock: &PeriodClock, now: DateTime<Utc>) -> anyhow::Result<Ledger> {
        let _ = (store, cfg, clock, now);
        todo!(
            "TODO(agent-budget): usage_by_tier for period and window, weights from cfg.models or default_weight, calibration from kv 'budget.calibration'"
        )
    }

    pub fn tier(&self, tier: ModelTier) -> TierLedger {
        self.tiers.iter().copied().find(|t| t.tier == tier).unwrap_or(TierLedger { tier, ..Default::default() })
    }

    /// Period spend as a fraction of the total budget, corrected by calibration.
    pub fn period_fraction(&self) -> f64 {
        let _ = &self.calibration;
        todo!("TODO(agent-budget)")
    }

    pub fn window_fraction(&self) -> f64 {
        if self.window_budget <= 0.0 { 1.0 } else { self.total_window_weighted / self.window_budget }
    }

    pub fn elapsed_fraction(&self) -> f64 {
        self.period.elapsed_fraction(self.now)
    }
}
