//! Spend so far, per model, for the current period and window of one
//! provider; [`Ledgers`] holds one ledger per enabled provider.

use std::collections::BTreeMap;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::BudgetConfig;
use crate::domain::{ModelTier, Provider, TokenUsage};
use crate::store::Store;

use super::period::{AnchorSource, Period, PeriodClock};
use super::probe::{ObservedUsage, apply_observed, load_observed};

/// kv key under which Claude's [`Calibration`] is stored (the name kept from
/// before budgets were per provider; see [`calibration_key`]).
pub const CALIBRATION_KEY: &str = "budget.calibration.claude";

/// kv key under which a provider's [`Calibration`] is stored.
pub fn calibration_key(provider: Provider) -> String {
    format!("budget.calibration.{provider}")
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
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

/// Snapshot of one provider's budget, used by the policy and the dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub provider: Provider,
    pub now: DateTime<Utc>,
    pub period: Period,
    pub window: Period,
    /// One entry per configured model (most capable first) plus any model
    /// that has usage rows this period but is not configured.
    pub tiers: Vec<TierLedger>,
    pub total_period_weighted: f64,
    pub total_window_weighted: f64,
    pub period_budget: f64,
    pub window_budget: f64,
    /// False when the provider has no rolling window (`window_hours = 0`):
    /// `window_fraction()` is 0 and window checks pass.
    #[serde(default = "default_true")]
    pub window_enabled: bool,
    /// Observed usage percentage (from `budget set-observed` or a probe), if
    /// taken inside the current period.
    pub calibration: Option<Calibration>,
    /// The provider's latest self-reported usage, if a probe stored one.
    #[serde(default)]
    pub observed: Option<ObservedUsage>,
    /// Where the period boundaries came from.
    #[serde(default)]
    pub anchor_source: AnchorSource,
}

fn default_true() -> bool {
    true
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

/// Cost weight for a model: the configured one, else 1.0 (Sonnet-equivalent)
/// for models that are not in the config.
pub fn tier_weight(cfg: &BudgetConfig, tier: &ModelTier) -> f64 {
    cfg.model_budget(tier).map(|m| m.weight).unwrap_or(1.0)
}

/// Budget share for a model: 0 when it is absent from config or disabled.
pub fn tier_share(cfg: &BudgetConfig, tier: &ModelTier) -> f64 {
    cfg.model_budget(tier).filter(|m| m.enabled).map(|m| m.share.max(0.0)).unwrap_or(0.0)
}

impl Ledger {
    /// Aggregate usage rows into a ledger for one provider.
    ///
    /// Builds the period clock from `cfg.providers.<provider>` (an observed
    /// `period_resets_at` in kv `budget.observed.<provider>` overrides the
    /// configured anchor), reads `usage` rows of the provider's models for
    /// the current period and the rolling window, applies the per-model cost
    /// weights and shares, and attaches the `budget.calibration.<provider>`
    /// kv entry when it was taken inside the current period (an older
    /// calibration refers to a different allowance and is ignored). A stored
    /// observation is folded in last ([`apply_observed`]). Fails only when
    /// the database cannot be read.
    pub fn load(store: &Store, cfg: &BudgetConfig, provider: Provider, now: DateTime<Utc>) -> anyhow::Result<Ledger> {
        let budget = cfg.provider(provider);
        let observed = load_observed(store, provider)?;
        let mut clock = PeriodClock::from_provider(budget, now);
        if let Some(reset) = observed.as_ref().and_then(|o| o.period_resets_at)
            && reset > now
        {
            clock = clock.with_observed_anchor(reset);
        }
        let period = clock.current_period(now);
        let window = clock.current_window(now);
        let period_rows: Vec<_> = store
            .usage_by_tier(period.start, period.end)
            .context("read period usage")?
            .into_iter()
            .filter(|r| r.tier.provider() == provider)
            .collect();
        let window_rows: Vec<_> = store
            .usage_by_tier(window.start, window.end)
            .context("read window usage")?
            .into_iter()
            .filter(|r| r.tier.provider() == provider)
            .collect();
        let period_budget = budget.period_weighted_tokens as f64;

        let mut models = cfg.models_for(provider);
        for row in &period_rows {
            if !models.contains(&row.tier) {
                models.push(row.tier.clone());
            }
        }
        let tiers: Vec<TierLedger> = models
            .into_iter()
            .map(|tier| {
                let weight = tier_weight(cfg, &tier);
                let p = period_rows.iter().find(|r| r.tier == tier).cloned().unwrap_or_default();
                let w = window_rows.iter().find(|r| r.tier == tier).cloned().unwrap_or_default();
                TierLedger {
                    period_usage: p.usage,
                    window_usage: w.usage,
                    period_weighted: p.usage.weighted() * weight,
                    window_weighted: w.usage.weighted() * weight,
                    period_budget: period_budget * tier_share(cfg, &tier),
                    messages: p.messages,
                    tier,
                }
            })
            .collect();

        let calibration = store
            .kv_get::<Calibration>(&calibration_key(provider))
            .context("read budget calibration")?
            .filter(|c| period.contains(c.at));

        let mut ledger = Ledger {
            provider,
            now,
            period,
            window,
            total_period_weighted: tiers.iter().map(|t| t.period_weighted).sum(),
            total_window_weighted: tiers.iter().map(|t| t.window_weighted).sum(),
            tiers,
            period_budget,
            window_budget: budget.window_weighted_tokens as f64,
            window_enabled: clock.has_window(),
            calibration,
            observed: None,
            anchor_source: clock.anchor_source,
        };
        if let Some(obs) = &observed {
            apply_observed(&mut ledger, obs);
        }
        Ok(ledger)
    }

    /// The entry for a model (zeroed when the model has no entry).
    pub fn tier(&self, tier: &ModelTier) -> TierLedger {
        self.tiers.iter().find(|t| t.tier == *tier).cloned().unwrap_or(TierLedger { tier: tier.clone(), ..Default::default() })
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

    /// Window spend as a fraction of the window budget; 0 without a window.
    pub fn window_fraction(&self) -> f64 {
        if !self.window_enabled {
            0.0
        } else if self.window_budget <= 0.0 {
            1.0
        } else {
            self.total_window_weighted / self.window_budget
        }
    }

    pub fn elapsed_fraction(&self) -> f64 {
        self.period.elapsed_fraction(self.now)
    }
}

/// One [`Ledger`] per enabled provider.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Ledgers {
    pub by_provider: BTreeMap<Provider, Ledger>,
}

impl Ledgers {
    /// Load a ledger for every enabled provider. Fails when any of them
    /// cannot be read from the database.
    pub fn load(store: &Store, cfg: &BudgetConfig, now: DateTime<Utc>) -> anyhow::Result<Ledgers> {
        let mut by_provider = BTreeMap::new();
        for provider in cfg.providers.enabled() {
            let ledger = Ledger::load(store, cfg, provider, now).with_context(|| format!("load {provider} ledger"))?;
            by_provider.insert(provider, ledger);
        }
        Ok(Ledgers { by_provider })
    }

    /// A set holding just one ledger (tests, single-provider call sites).
    pub fn single(ledger: Ledger) -> Ledgers {
        let mut by_provider = BTreeMap::new();
        by_provider.insert(ledger.provider, ledger);
        Ledgers { by_provider }
    }

    pub fn get(&self, provider: Provider) -> Option<&Ledger> {
        self.by_provider.get(&provider)
    }

    pub fn get_mut(&mut self, provider: Provider) -> Option<&mut Ledger> {
        self.by_provider.get_mut(&provider)
    }

    /// The ledger of the provider that runs `model`, if that provider is enabled.
    pub fn for_model(&self, model: &ModelTier) -> Option<&Ledger> {
        self.get(model.provider())
    }

    pub fn for_model_mut(&mut self, model: &ModelTier) -> Option<&mut Ledger> {
        self.get_mut(model.provider())
    }

    /// Ledgers in `order` (providers missing from `order` follow in
    /// `Provider::ALL` order).
    pub fn ordered(&self, order: &[Provider]) -> Vec<&Ledger> {
        let mut out: Vec<&Ledger> = Vec::new();
        for p in order.iter().chain(Provider::ALL.iter()) {
            if let Some(l) = self.by_provider.get(p)
                && !out.iter().any(|x| x.provider == *p)
            {
                out.push(l);
            }
        }
        out
    }

    /// The soonest period end across providers (`None` when empty).
    pub fn earliest_period_end(&self) -> Option<DateTime<Utc>> {
        self.by_provider.values().map(|l| l.period.end).min()
    }

    pub fn is_empty(&self) -> bool {
        self.by_provider.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (Provider, &Ledger)> {
        self.by_provider.iter().map(|(p, l)| (*p, l))
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::budget::probe::save_observed;
    use crate::domain::{Task, TaskId, TaskSource, UsageRecord};

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn cfg() -> BudgetConfig {
        let mut cfg = BudgetConfig::default();
        cfg.providers.claude.period_anchor = Some("2026-09-28T00:00:00Z".into());
        cfg
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
        let ledger = Ledger::load(&store, &cfg, Provider::Claude, now).unwrap();
        assert_eq!(ledger.provider, Provider::Claude);
        assert_eq!(ledger.tiers.len(), 4);
        assert_eq!(
            ledger.tiers.iter().map(|t| t.tier.clone()).collect::<Vec<_>>(),
            vec![ModelTier::fable(), ModelTier::opus(), ModelTier::sonnet(), ModelTier::haiku()],
            "most capable first"
        );
        assert_eq!(ledger.total_period_weighted, 0.0);
        assert_eq!(ledger.period_fraction(), 0.0);
        assert_eq!(ledger.window_fraction(), 0.0);
        assert_eq!(ledger.period_budget, 80_000_000.0);
        assert_eq!(ledger.window_budget, 12_000_000.0);
        assert!(ledger.window_enabled);
        assert_eq!(ledger.anchor_source, AnchorSource::Config);
        assert!((ledger.tier(&ModelTier::fable()).period_budget - 20_000_000.0).abs() < 1e-6);
        assert!((ledger.tier(&ModelTier::haiku()).period_budget - 4_000_000.0).abs() < 1e-6);
        assert!(ledger.calibration.is_none());
        assert!(ledger.observed.is_none());
        assert_eq!(ledger.tier(&ModelTier::fable()).period_remaining(), 20_000_000.0);
        // The JSON shape keeps the fields `budget show --json` consumers know.
        let json = serde_json::to_value(&ledger).unwrap();
        assert_eq!(json["period_budget"].as_f64(), Some(80_000_000.0));
        assert_eq!(json["provider"], "claude");
        assert_eq!(json["anchor_source"], "config");
    }

    #[test]
    fn usage_is_weighted_per_tier_and_split_by_period_and_window() {
        let (store, task) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let cfg = cfg();
        // 100 output tokens = 500 weighted; Fable weight 5 → 2500; inside window.
        usage(&store, task, "m1", ModelTier::fable(), 100, now - Duration::hours(1));
        // Sonnet weight 1 → 1000, inside the period but outside the 5h window.
        usage(&store, task, "m2", ModelTier::sonnet(), 200, now - Duration::hours(10));
        // Previous period: ignored.
        usage(&store, task, "m3", ModelTier::opus(), 1000, now - Duration::days(10));
        // Another provider's usage never lands in Claude's ledger.
        usage(&store, task, "m4", ModelTier::new("gpt-6.1-sol"), 1000, now - Duration::hours(1));
        let ledger = Ledger::load(&store, &cfg, Provider::Claude, now).unwrap();
        let fable = ledger.tier(&ModelTier::fable());
        assert_eq!(fable.period_weighted, 2500.0);
        assert_eq!(fable.window_weighted, 2500.0);
        assert_eq!(fable.messages, 1);
        let sonnet = ledger.tier(&ModelTier::sonnet());
        assert_eq!(sonnet.period_weighted, 1000.0);
        assert_eq!(sonnet.window_weighted, 0.0);
        assert_eq!(ledger.tier(&ModelTier::opus()).period_weighted, 0.0);
        assert_eq!(ledger.total_period_weighted, 3500.0);
        assert_eq!(ledger.total_window_weighted, 2500.0);
        assert!((ledger.period_fraction() - 3500.0 / 80_000_000.0).abs() < 1e-12);
        assert_eq!(ledger.tiers.len(), 4);
    }

    #[test]
    fn disabled_or_unconfigured_tiers_get_zero_budget_and_unit_weight() {
        let (store, task) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let mut cfg = cfg();
        cfg.providers.claude.models.get_mut(&ModelTier::opus()).unwrap().enabled = false;
        cfg.providers.claude.models.remove(&ModelTier::haiku());
        usage(&store, task, "m1", ModelTier::haiku(), 100, now - Duration::hours(1));
        let ledger = Ledger::load(&store, &cfg, Provider::Claude, now).unwrap();
        assert_eq!(ledger.tier(&ModelTier::opus()).period_budget, 0.0);
        assert_eq!(ledger.tier(&ModelTier::opus()).period_spent_fraction(), 1.0);
        let haiku = ledger.tier(&ModelTier::haiku());
        assert_eq!(haiku.period_budget, 0.0);
        assert!((haiku.period_weighted - 500.0).abs() < 1e-9, "unconfigured models count with weight 1.0");
        assert_eq!(ledger.tiers.len(), 4, "usage on an unconfigured model still shows up");
        assert_eq!(ledger.tiers.last().unwrap().tier, ModelTier::haiku());
    }

    #[test]
    fn calibration_applies_only_inside_the_current_period() {
        let (store, task) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let cfg = cfg();
        usage(&store, task, "m1", ModelTier::sonnet(), 1_600_000, now - Duration::hours(1)); // 8M weighted = 10%
        store
            .kv_set(
                CALIBRATION_KEY,
                &Calibration { observed_fraction: 0.35, at: now - Duration::hours(2), measured_fraction: 0.05 },
            )
            .unwrap();
        let ledger = Ledger::load(&store, &cfg, Provider::Claude, now).unwrap();
        assert!(ledger.calibration.is_some());
        assert!((ledger.measured_period_fraction() - 0.10).abs() < 1e-9);
        // offset = 0.35 - 0.05 = 0.30 → corrected 0.40
        assert!((ledger.period_fraction() - 0.40).abs() < 1e-9);

        store
            .kv_set(
                &calibration_key(Provider::Claude),
                &Calibration { observed_fraction: 0.9, at: now - Duration::days(10), measured_fraction: 0.0 },
            )
            .unwrap();
        let ledger = Ledger::load(&store, &cfg, Provider::Claude, now).unwrap();
        assert!(ledger.calibration.is_none(), "stale calibration from a previous period is ignored");
        assert!((ledger.period_fraction() - 0.10).abs() < 1e-9);
    }

    #[test]
    fn observed_usage_moves_the_period_and_calibrates() {
        let (store, task) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let cfg = cfg();
        usage(&store, task, "m1", ModelTier::sonnet(), 1_600_000, now - Duration::hours(1)); // 10%
        let obs = ObservedUsage {
            period_used: Some(0.5),
            period_resets_at: Some(at("2026-10-03T08:00:00Z")),
            ..ObservedUsage::empty(now - Duration::minutes(10))
        };
        save_observed(&store, Provider::Claude, &obs).unwrap();
        let ledger = Ledger::load(&store, &cfg, Provider::Claude, now).unwrap();
        assert_eq!(ledger.period.end, at("2026-10-03T08:00:00Z"));
        assert_eq!(ledger.period.start, at("2026-09-26T08:00:00Z"));
        assert_eq!(ledger.anchor_source, AnchorSource::Observed);
        assert!((ledger.period_fraction() - 0.5).abs() < 1e-9, "observed period usage calibrates");
        assert_eq!(ledger.observed, Some(obs));
    }

    #[test]
    fn no_window_means_zero_window_fraction() {
        let (store, task) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let mut cfg = cfg();
        cfg.providers.claude.window_hours = 0;
        usage(&store, task, "m1", ModelTier::sonnet(), 100, now - Duration::minutes(1));
        let ledger = Ledger::load(&store, &cfg, Provider::Claude, now).unwrap();
        assert!(!ledger.window_enabled);
        assert_eq!(ledger.total_window_weighted, 0.0);
        assert_eq!(ledger.window_fraction(), 0.0);
        assert_eq!(ledger.total_period_weighted, 500.0);
    }

    #[test]
    fn ledgers_cover_enabled_providers_in_order() {
        let (store, _) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let mut cfg = cfg();
        let ledgers = Ledgers::load(&store, &cfg, now).unwrap();
        assert_eq!(ledgers.by_provider.keys().copied().collect::<Vec<_>>(), vec![Provider::Claude]);
        assert!(ledgers.get(Provider::Codex).is_none());
        assert!(ledgers.for_model(&ModelTier::new("gpt-6-luna")).is_none());
        assert_eq!(ledgers.for_model(&ModelTier::opus()).map(|l| l.provider), Some(Provider::Claude));

        cfg.providers.codex.enabled = true;
        cfg.providers.codex.period_anchor = Some("2026-09-30T00:00:00Z".into());
        cfg.providers.gemini.enabled = true;
        let mut ledgers = Ledgers::load(&store, &cfg, now).unwrap();
        assert_eq!(ledgers.by_provider.len(), 3);
        assert_eq!(ledgers.get(Provider::Codex).unwrap().tiers.len(), 3);
        assert_eq!(ledgers.get(Provider::Codex).unwrap().period_budget, 60_000_000.0);
        let order: Vec<Provider> = ledgers.ordered(&[Provider::Gemini]).iter().map(|l| l.provider).collect();
        assert_eq!(order, vec![Provider::Gemini, Provider::Claude, Provider::Codex]);
        // Claude's period ends 2026-10-05; Codex's 2026-10-07; Gemini defaults to Monday → 2026-10-05.
        assert_eq!(ledgers.earliest_period_end(), Some(at("2026-10-05T00:00:00Z")));
        ledgers.get_mut(Provider::Codex).unwrap().total_period_weighted = 1.0;
        assert_eq!(ledgers.get(Provider::Codex).unwrap().total_period_weighted, 1.0);
        assert!(Ledgers::default().earliest_period_end().is_none());
        assert_eq!(Ledgers::single(ledgers.get(Provider::Gemini).unwrap().clone()).iter().count(), 1);
    }

    #[test]
    fn period_fraction_is_clamped() {
        let now = at("2026-10-01T12:00:00Z");
        let period = Period { start: now - Duration::days(1), end: now + Duration::days(6) };
        let mut ledger = Ledger {
            provider: Provider::Claude,
            now,
            period,
            window: Period { start: now - Duration::hours(5), end: now },
            tiers: vec![],
            total_period_weighted: 10.0,
            total_window_weighted: 0.0,
            period_budget: 100.0,
            window_budget: 100.0,
            window_enabled: true,
            calibration: Some(Calibration { observed_fraction: 0.0, at: now, measured_fraction: 0.5 }),
            observed: None,
            anchor_source: AnchorSource::Default,
        };
        assert_eq!(ledger.period_fraction(), 0.0, "negative offsets cannot go below zero");
        ledger.calibration = Some(Calibration { observed_fraction: 5.0, at: now, measured_fraction: 0.0 });
        assert_eq!(ledger.period_fraction(), 2.0);
        ledger.period_budget = 0.0;
        ledger.calibration = None;
        assert_eq!(ledger.period_fraction(), 1.0, "zero budget counts as spent");
        assert_eq!(tier_weight(&BudgetConfig::default(), &ModelTier::new("codex:custom")), 1.0);
        assert_eq!(tier_share(&BudgetConfig::default(), &ModelTier::new("codex:custom")), 0.0);
        assert_eq!(tier_weight(&BudgetConfig::default(), &ModelTier::fable()), 5.0);
        assert_eq!(tier_share(&BudgetConfig::default(), &ModelTier::fable()), 0.25);
    }
}
