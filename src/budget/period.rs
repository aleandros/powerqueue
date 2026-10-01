//! Usage periods: a fixed-length period anchored at a known reset instant,
//! and a rolling window.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::BudgetConfig;

/// A half-open interval `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Period {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl Period {
    pub fn contains(&self, t: DateTime<Utc>) -> bool {
        t >= self.start && t < self.end
    }
    pub fn len(&self) -> Duration {
        self.end - self.start
    }
    /// 0.0 at start, 1.0 at end (clamped).
    pub fn elapsed_fraction(&self, now: DateTime<Utc>) -> f64 {
        let total = self.len().num_seconds().max(1) as f64;
        ((now - self.start).num_seconds() as f64 / total).clamp(0.0, 1.0)
    }
    pub fn remaining(&self, now: DateTime<Utc>) -> Duration {
        (self.end - now).max(Duration::zero())
    }
}

/// Computes periods from config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodClock {
    pub period: Duration,
    pub window: Duration,
    /// A known period start; periods repeat every `period` before and after it.
    pub anchor: DateTime<Utc>,
}

impl PeriodClock {
    /// Build from config. Without an anchor we default to Monday 00:00 UTC of
    /// the current week, which is wrong for most accounts but stable; `doctor`
    /// nags until `budget set-reset` is used.
    pub fn from_config(cfg: &BudgetConfig, now: DateTime<Utc>) -> Self {
        let _ = (cfg, now);
        todo!("TODO(agent-budget)")
    }

    /// The period containing `now`.
    pub fn current_period(&self, now: DateTime<Utc>) -> Period {
        let _ = now;
        todo!("TODO(agent-budget): floor((now - anchor) / period)")
    }

    /// The rolling window ending at `now`.
    pub fn current_window(&self, now: DateTime<Utc>) -> Period {
        Period { start: now - self.window, end: now }
    }
}
