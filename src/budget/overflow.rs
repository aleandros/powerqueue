//! Conservative paid-usage estimates, never a provider billing statement.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::BudgetConfig;
use crate::domain::Provider;

use super::ledger::Ledger;

/// Sonnet-equivalent dollars per weighted token. Model weights and the token
/// mix come from the existing ledger, so this is a planning estimate, not pricing.
pub const USD_PER_WEIGHTED_TOKEN: f64 = 3.0 / 1_000_000.0;

/// Whether this provider explicitly permits credit-funded launches.
pub fn enabled(cfg: &BudgetConfig, provider: Provider) -> bool {
    provider == Provider::Claude && cfg.provider(provider).allow_overflow
}

/// Observation cooldown with the opt-in exemption applied. Explicit blocked
/// readings remain authoritative even when subscription percentages are ignored.
pub fn observed_cooldown(cfg: &BudgetConfig, provider: Provider, obs: &super::ObservedUsage) -> Option<DateTime<Utc>> {
    if enabled(cfg, provider) && !obs.blocked { None } else { obs.cooldown_until() }
}

/// Persisted first entry into credit allocation. The timestamp is retained
/// across restarts/config reloads and expires with its subscription period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub started_at: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
}

impl Entry {
    /// Fold an authoritative reset into the durable entry, including expiry.
    /// A later observation in a new period must never revive an old entry.
    #[must_use]
    pub fn refreshed(&self, ledger: &Ledger) -> Self {
        let mut entry = self.clone();
        if let Some(reset) = ledger.observed.as_ref().and_then(|o| o.period_resets_at).filter(|r| *r > self.started_at) {
            entry.period_end = if reset <= ledger.now || ledger.period.contains(self.started_at) {
                reset
            } else {
                self.period_end.min(ledger.period.start)
            };
        }
        entry
    }

    /// Whether this allocation still belongs to the current credit period.
    pub fn current(&self, ledger: &Ledger) -> bool {
        ledger.now < self.refreshed(ledger).period_end
    }
}

/// Overflow state displayed in budget JSON, the CLI and the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Overflow {
    pub enabled: bool,
    pub max_usd: Option<f64>,
    pub started_at: Option<DateTime<Utc>>,
    /// All recorded Claude spend since first entry this period, conservatively
    /// including any later subscription-funded usage after a window reset.
    pub estimated_spend_usd: f64,
    /// Predicted costs reserved by this launch pass (never persisted as spend).
    pub reserved_usd: f64,
}

impl Overflow {
    /// A concise status, including the estimate and optional cap.
    pub fn description(&self) -> String {
        let state = if !self.enabled {
            "off"
        } else if self.max_usd.is_some_and(|cap| self.estimated_spend_usd >= cap) {
            "cap reached"
        } else if self.started_at.is_some() {
            "active"
        } else {
            "ready"
        };
        let cap = self.max_usd.map_or_else(|| "uncapped".to_string(), |v| format!("cap ${v:.2}"));
        format!("overflow {state}: credits ~${:.2} ({cap}, ledger estimate)", self.estimated_spend_usd)
    }
}

impl Ledger {
    /// Which subscription gate would a tier-weighted cost exceed? The same
    /// safety boundary governs admission and reservation accounting.
    pub fn overflow_reason(&self, cfg: &BudgetConfig, cost: f64) -> Option<&'static str> {
        let margin = (1.0 - cfg.safety_margin).max(0.0);
        if self.period_fraction() + cost / self.period_budget.max(1.0) > margin {
            Some("period allowance exhausted")
        } else if self.has_window() && self.window_fraction() + cost / self.window_divisor() > margin {
            Some("window allowance exhausted")
        } else {
            None
        }
    }
}
