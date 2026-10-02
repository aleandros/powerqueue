//! Spend so far, per tier, for the current period and window.

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::BudgetConfig;
use crate::domain::{ModelTier, TokenUsage};
use crate::store::Store;

use super::period::{Period, PeriodClock};

/// kv key under which [`Calibration`] is stored.
pub const CALIBRATION_KEY: &str = "budget.calibration";

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

impl Calibration {
    /// `observed - measured` at calibration time: the share of the period
    /// budget consumed outside powerqueue (negative when our weights overestimate).
    pub fn offset(&self) -> f64 {
        self.observed_fraction - self.measured_fraction
    }
}

/// Cost weight for a tier: the configured one, else the built-in default.
pub fn tier_weight(cfg: &BudgetConfig, tier: ModelTier) -> f64 {
    cfg.models.get(&tier).map(|m| m.weight).unwrap_or_else(|| tier.default_weight())
}

/// Budget share for a tier: 0 when the tier is absent from config or disabled.
pub fn tier_share(cfg: &BudgetConfig, tier: ModelTier) -> f64 {
    cfg.models.get(&tier).filter(|m| m.enabled).map(|m| m.share.max(0.0)).unwrap_or(0.0)
}

impl Ledger {
    /// Aggregate usage rows into a ledger.
    ///
    /// Reads `usage` for the current period and the rolling window, applies
    /// the per-tier cost weights and shares from `cfg`, and attaches the
    /// `budget.calibration` kv entry when it was taken inside the current
    /// period (an older calibration refers to a different allowance and is
    /// ignored). Fails only when the database cannot be read.
    pub fn load(store: &Store, cfg: &BudgetConfig, clock: &PeriodClock, now: DateTime<Utc>) -> anyhow::Result<Ledger> {
        let period = clock.current_period(now);
        let window = clock.current_window(now);
        let period_rows = store.usage_by_tier(period.start, period.end).context("read period usage")?;
        let window_rows = store.usage_by_tier(window.start, window.end).context("read window usage")?;
        let period_budget = cfg.period_weighted_tokens as f64;

        let tiers: Vec<TierLedger> = ModelTier::ALL
            .iter()
            .map(|&tier| {
                let weight = tier_weight(cfg, tier);
                let p = period_rows.iter().find(|r| r.tier == tier).copied().unwrap_or_default();
                let w = window_rows.iter().find(|r| r.tier == tier).copied().unwrap_or_default();
                TierLedger {
                    tier,
                    period_usage: p.usage,
                    window_usage: w.usage,
                    period_weighted: p.usage.weighted() * weight,
                    window_weighted: w.usage.weighted() * weight,
                    period_budget: period_budget * tier_share(cfg, tier),
                    messages: p.messages,
                }
            })
            .collect();

        let calibration =
            store.kv_get::<Calibration>(CALIBRATION_KEY).context("read budget calibration")?.filter(|c| period.contains(c.at));

        Ok(Ledger {
            now,
            period,
            window,
            total_period_weighted: tiers.iter().map(|t| t.period_weighted).sum(),
            total_window_weighted: tiers.iter().map(|t| t.window_weighted).sum(),
            tiers,
            period_budget,
            window_budget: cfg.window_weighted_tokens as f64,
            calibration,
        })
    }

    pub fn tier(&self, tier: ModelTier) -> TierLedger {
        self.tiers.iter().copied().find(|t| t.tier == tier).unwrap_or(TierLedger { tier, ..Default::default() })
    }

    /// Period spend as a fraction of the total budget, without calibration.
    /// A zero budget counts as fully spent.
    pub fn measured_period_fraction(&self) -> f64 {
        if self.period_budget <= 0.0 { 1.0 } else { (self.total_period_weighted / self.period_budget).max(0.0) }
    }

    /// Period spend as a fraction of the total budget, corrected by calibration.
    ///
    /// When the user reported `/usage` showing X% at a time we had measured
    /// m, the difference `X - m` is usage we cannot see (other tools,
    /// interactive sessions) or a systematic error in our weights. That offset
    /// is added to the current measurement. The result is clamped to `[0, 2]`
    /// so a stale or mistyped calibration cannot produce nonsense.
    pub fn period_fraction(&self) -> f64 {
        let measured = self.measured_period_fraction();
        let corrected = match self.calibration {
            Some(c) => measured + c.offset(),
            None => measured,
        };
        corrected.clamp(0.0, 2.0)
    }

    pub fn window_fraction(&self) -> f64 {
        if self.window_budget <= 0.0 { 1.0 } else { self.total_window_weighted / self.window_budget }
    }

    pub fn elapsed_fraction(&self) -> f64 {
        self.period.elapsed_fraction(self.now)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::domain::{Task, TaskId, TaskSource, UsageRecord};

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn cfg() -> BudgetConfig {
        BudgetConfig { period_anchor: Some("2026-09-28T00:00:00Z".into()), ..BudgetConfig::default() }
    }

    fn store_with_task() -> (Store, TaskId) {
        let store = Store::open_in_memory().unwrap();
        let task = Task::new("ENG-1", "t", TaskSource::Manual);
        store.insert_task(&task).unwrap();
        (store, task.id)
    }

    fn usage(store: &Store, task_id: TaskId, id: &str, tier: ModelTier, output: u64, at: DateTime<Utc>) {
        let rec = UsageRecord {
            session_id: uuid::Uuid::new_v4(),
            task_id,
            message_id: id.to_string(),
            model_id: format!("claude-{}-5", tier.alias()),
            tier,
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: output,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
            timestamp: at,
        };
        assert!(store.record_usage(&rec).unwrap());
    }

    #[test]
    fn empty_store_gives_zero_spend_and_full_budgets() {
        let (store, _) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let cfg = cfg();
        let clock = PeriodClock::from_config(&cfg, now);
        let ledger = Ledger::load(&store, &cfg, &clock, now).unwrap();
        assert_eq!(ledger.tiers.len(), 4);
        assert_eq!(ledger.total_period_weighted, 0.0);
        assert_eq!(ledger.period_fraction(), 0.0);
        assert_eq!(ledger.window_fraction(), 0.0);
        assert_eq!(ledger.period_budget, 60_000_000.0);
        assert_eq!(ledger.window_budget, 4_000_000.0);
        assert!((ledger.tier(ModelTier::Fable).period_budget - 15_000_000.0).abs() < 1e-6);
        assert!((ledger.tier(ModelTier::Haiku).period_budget - 3_000_000.0).abs() < 1e-6);
        assert!(ledger.calibration.is_none());
        assert_eq!(ledger.tier(ModelTier::Fable).period_remaining(), 15_000_000.0);
    }

    #[test]
    fn usage_is_weighted_per_tier_and_split_by_period_and_window() {
        let (store, task) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let cfg = cfg();
        let clock = PeriodClock::from_config(&cfg, now);
        // 100 output tokens = 500 weighted; Fable weight 5 → 2500; inside window.
        usage(&store, task, "m1", ModelTier::Fable, 100, now - Duration::hours(1));
        // Sonnet weight 1 → 1000, inside the period but outside the 5h window.
        usage(&store, task, "m2", ModelTier::Sonnet, 200, now - Duration::hours(10));
        // Previous period: ignored.
        usage(&store, task, "m3", ModelTier::Opus, 1000, now - Duration::days(10));
        let ledger = Ledger::load(&store, &cfg, &clock, now).unwrap();
        let fable = ledger.tier(ModelTier::Fable);
        assert_eq!(fable.period_weighted, 2500.0);
        assert_eq!(fable.window_weighted, 2500.0);
        assert_eq!(fable.messages, 1);
        let sonnet = ledger.tier(ModelTier::Sonnet);
        assert_eq!(sonnet.period_weighted, 1000.0);
        assert_eq!(sonnet.window_weighted, 0.0);
        assert_eq!(ledger.tier(ModelTier::Opus).period_weighted, 0.0);
        assert_eq!(ledger.total_period_weighted, 3500.0);
        assert_eq!(ledger.total_window_weighted, 2500.0);
        assert!((ledger.period_fraction() - 3500.0 / 60_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn disabled_or_missing_tiers_get_zero_budget_but_keep_default_weight() {
        let (store, task) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let mut cfg = cfg();
        cfg.models.get_mut(&ModelTier::Opus).unwrap().enabled = false;
        cfg.models.remove(&ModelTier::Haiku);
        let clock = PeriodClock::from_config(&cfg, now);
        usage(&store, task, "m1", ModelTier::Haiku, 100, now - Duration::hours(1));
        let ledger = Ledger::load(&store, &cfg, &clock, now).unwrap();
        assert_eq!(ledger.tier(ModelTier::Opus).period_budget, 0.0);
        assert_eq!(ledger.tier(ModelTier::Opus).period_spent_fraction(), 1.0);
        let haiku = ledger.tier(ModelTier::Haiku);
        assert_eq!(haiku.period_budget, 0.0);
        assert!((haiku.period_weighted - 500.0 * ModelTier::Haiku.default_weight()).abs() < 1e-9);
    }

    #[test]
    fn calibration_applies_only_inside_the_current_period() {
        let (store, task) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let cfg = cfg();
        let clock = PeriodClock::from_config(&cfg, now);
        usage(&store, task, "m1", ModelTier::Sonnet, 1_200_000, now - Duration::hours(1)); // 6M weighted = 10%
        store
            .kv_set(
                CALIBRATION_KEY,
                &Calibration { observed_fraction: 0.35, at: now - Duration::hours(2), measured_fraction: 0.05 },
            )
            .unwrap();
        let ledger = Ledger::load(&store, &cfg, &clock, now).unwrap();
        assert!(ledger.calibration.is_some());
        assert!((ledger.measured_period_fraction() - 0.10).abs() < 1e-9);
        // offset = 0.35 - 0.05 = 0.30 → corrected 0.40
        assert!((ledger.period_fraction() - 0.40).abs() < 1e-9);

        store
            .kv_set(
                CALIBRATION_KEY,
                &Calibration { observed_fraction: 0.9, at: now - Duration::days(10), measured_fraction: 0.0 },
            )
            .unwrap();
        let ledger = Ledger::load(&store, &cfg, &clock, now).unwrap();
        assert!(ledger.calibration.is_none(), "stale calibration from a previous period is ignored");
        assert!((ledger.period_fraction() - 0.10).abs() < 1e-9);
    }

    #[test]
    fn period_fraction_is_clamped() {
        let now = at("2026-10-01T12:00:00Z");
        let period = Period { start: now - Duration::days(1), end: now + Duration::days(6) };
        let mut ledger = Ledger {
            now,
            period,
            window: Period { start: now - Duration::hours(5), end: now },
            tiers: vec![],
            total_period_weighted: 10.0,
            total_window_weighted: 0.0,
            period_budget: 100.0,
            window_budget: 100.0,
            calibration: Some(Calibration { observed_fraction: 0.0, at: now, measured_fraction: 0.5 }),
        };
        assert_eq!(ledger.period_fraction(), 0.0, "negative offsets cannot go below zero");
        ledger.calibration = Some(Calibration { observed_fraction: 5.0, at: now, measured_fraction: 0.0 });
        assert_eq!(ledger.period_fraction(), 2.0);
        ledger.period_budget = 0.0;
        ledger.calibration = None;
        assert_eq!(ledger.period_fraction(), 1.0, "zero budget counts as spent");
    }
}
