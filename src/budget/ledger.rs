use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{BudgetConfig, ProviderBudget};
use crate::domain::{ModelTier, Provider, TokenUsage};
use crate::store::TierUsage;

use super::period::{AnchorSource, Period, PeriodClock};
use super::probe::{ObservationSample, ObservedUsage, apply_observed};

/// kv key of the pre-0.7 Claude calibration (an additive offset). No longer
/// read: the ledger learns a rate from the observation history instead.
pub const CALIBRATION_KEY: &str = "budget.calibration.claude";

/// kv key of a provider's pre-0.7 calibration (see [`CALIBRATION_KEY`]).
pub fn calibration_key(provider: Provider) -> String {
    format!("budget.calibration.{provider}")
}

/// Two readings must differ by at least this much (2 points) before a rate
/// is learned from them.
pub const MIN_OBSERVED_DELTA: f64 = 0.02;
/// At least this many weighted tokens must have been measured between the two
/// readings: a rate learned from a trickle of tokens against a jump caused by
/// usage outside powerqueue would be absurdly small.
pub const MIN_MEASURED_DELTA: f64 = 1_000_000.0;
/// Window rates are learned from readings at most this far apart, so what
/// rolled out of the window in between stays small.
pub const WINDOW_LEARN_SPAN: Duration = Duration::hours(1);
/// A window reading without a reset instant is trusted for this long after
/// it was taken (the provider windows this code knows are 5 hours).
pub const OBSERVED_WINDOW_TTL: Duration = Duration::hours(5);

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TierLedger {
    pub tier: ModelTier,
    pub period_usage: TokenUsage,
    pub window_usage: TokenUsage,
    /// Weighted tokens spent this period (usage.weighted() × tier weight).
    pub period_weighted: f64,
    pub window_weighted: f64,
    /// Weighted budget for this tier in the period (`share × period_budget`,
    /// the effective, learned-or-configured budget).
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

/// How many weighted tokens one whole allowance (100%) is worth, learned
/// from two readings of the provider's own percentage and what we measured
/// in between: `budget = Δmeasured / Δobserved`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RateEstimate {
    /// Weighted tokens per 100% of the allowance.
    pub budget: f64,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// Change of the provider's percentage between `from` and `to` (fraction).
    pub observed_delta: f64,
    /// Weighted tokens we recorded between `from` and `to`.
    pub measured_delta: f64,
    /// Readings available in the span.
    pub samples: usize,
}

/// The learned exchange rates of a provider (`None` = not enough readings
/// yet; the configured budgets apply).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct LearnedRate {
    pub period: Option<RateEstimate>,
    pub window: Option<RateEstimate>,
}

/// Learn one rate from readings (oldest first). Every pair of readings
/// whose percentage grew by at least [`MIN_OBSERVED_DELTA`] with at least
/// [`MIN_MEASURED_DELTA`] weighted tokens recorded between them is a
/// candidate (`budget = Δmeasured / Δobserved`); the pair with the
/// **largest** budget wins. Usage powerqueue cannot see (another terminal)
/// only ever moves the percentage *more* than we measured, so every
/// candidate is a lower bound on the true allowance and the largest one is
/// the least contaminated. Readings after a reset (percentage went down)
/// never pair with earlier ones. With `max_span`, only pairs at most that
/// far apart count (window learning). Pure apart from `measured`.
pub fn learn_rate(
    samples: &[ObservationSample],
    value: impl Fn(&ObservationSample) -> Option<f64>,
    max_span: Option<Duration>,
    measured: impl Fn(DateTime<Utc>, DateTime<Utc>) -> f64,
) -> Option<RateEstimate> {
    let with: Vec<(DateTime<Utc>, f64)> = samples.iter().filter_map(|s| value(s).map(|v| (s.at, v))).collect();
    let mut best: Option<RateEstimate> = None;
    for (j, &(to, later)) in with.iter().enumerate() {
        // Walk back through the readings while the percentage never went
        // down: a decrease is a reset, and tokens burned before it must not
        // be paired with the percentage after it.
        for i in (0..j).rev() {
            let (from, earlier) = with[i];
            if earlier > with[i + 1].1 {
                break;
            }
            if max_span.is_some_and(|span| to - from > span) {
                break;
            }
            let observed_delta = later - earlier;
            if observed_delta < MIN_OBSERVED_DELTA {
                continue;
            }
            let measured_delta = measured(from, to);
            if measured_delta < MIN_MEASURED_DELTA {
                continue;
            }
            let budget = measured_delta / observed_delta;
            if best.is_none_or(|b| budget > b.budget) {
                best = Some(RateEstimate {
                    budget,
                    from,
                    to,
                    observed_delta,
                    measured_delta,
                    samples: with.iter().filter(|(at, _)| *at >= from && *at <= to).count(),
                });
            }
        }
    }
    best
}

/// Tier-weighted spend of one provider as a running total per minute, so
/// the spend between any two instants is two lookups.
pub struct SpendSeries {
    /// `(minute start, cumulative weighted tokens through the end of that minute)`, oldest first.
    points: Vec<(DateTime<Utc>, f64)>,
}

impl SpendSeries {
    /// Build from per-minute usage rows of `provider`'s models (any order).
    pub fn new(cfg: &BudgetConfig, provider: Provider, rows: &[(DateTime<Utc>, crate::store::TierUsage)]) -> Self {
        let mut per_minute: BTreeMap<DateTime<Utc>, f64> = BTreeMap::new();
        for (at, row) in rows.iter().filter(|(_, r)| r.tier.provider() == provider) {
            *per_minute.entry(*at).or_default() += row.usage.weighted() * tier_weight(cfg, &row.tier);
        }
        let mut total = 0.0;
        let points = per_minute
            .into_iter()
            .map(|(at, w)| {
                total += w;
                (at, total)
            })
            .collect();
        Self { points }
    }

    /// Weighted tokens recorded in minutes that start before `at`.
    pub fn until(&self, at: DateTime<Utc>) -> f64 {
        let n = self.points.partition_point(|(start, _)| *start < at);
        if n == 0 { 0.0 } else { self.points[n - 1].1 }
    }

    /// Weighted tokens recorded in `[from, to)` at minute granularity.
    pub fn between(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
        (self.until(to) - self.until(from)).max(0.0)
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
    /// Effective period budget in weighted tokens: learned when the
    /// observation history allows it, else `period_weighted_tokens`.
    pub period_budget: f64,
    /// Effective window budget (learned, else `window_weighted_tokens`).
    pub window_budget: f64,
    /// `budget.providers.<p>.period_weighted_tokens` as configured.
    #[serde(default)]
    pub configured_period_budget: f64,
    #[serde(default)]
    pub configured_window_budget: f64,
    /// False when the provider has no rolling window (`window_hours = 0`
    /// and no observation reports one): `window_fraction()` is 0 and
    /// window checks pass.
    #[serde(default = "default_true")]
    pub window_enabled: bool,
    /// Rates learned from the observation history (see [`LearnedRate`]).
    #[serde(default)]
    pub learned: LearnedRate,
    /// The provider's latest self-reported usage, if a probe stored one.
    #[serde(default)]
    pub observed: Option<ObservedUsage>,
    /// Weighted tokens recorded at or after `observed.observed_at` (the part
    /// of our measurement the observation does not cover yet).
    #[serde(default)]
    pub spent_since_observation: f64,
    /// Where the period boundaries came from.
    #[serde(default)]
    pub anchor_source: AnchorSource,
}

fn default_true() -> bool {
    true
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

/// Tier-weighted tokens of `provider`'s models in a set of usage rows.
fn weighted_sum(cfg: &BudgetConfig, provider: Provider, rows: &[TierUsage]) -> f64 {
    rows.iter().filter(|r| r.tier.provider() == provider).map(|r| r.usage.weighted() * tier_weight(cfg, &r.tier)).sum()
}

/// Where a provider's period and window sit: the configured anchor, unless
/// the latest observation reports a `period_resets_at` still ahead of
/// `now`, which wins ([`super::io`] loads the observation; the dashboard and
/// simulations may pass their own).
pub fn resolve_clock(budget: &ProviderBudget, observed: Option<&ObservedUsage>, now: DateTime<Utc>) -> PeriodClock {
    let clock = PeriodClock::from_provider(budget, now);
    match observed.and_then(|o| o.period_resets_at).filter(|reset| *reset > now) {
        Some(reset) => clock.with_observed_anchor(reset),
        None => clock,
    }
}

/// Everything [`Ledger::build`] needs from storage, as plain rows. The usage
/// rows must cover the period and window of the clock the ledger is built
/// with (see [`super::io`] for how they are read); rows of other providers'
/// models are ignored.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LedgerSource {
    /// The provider's latest self-reported usage, if a probe stored one.
    pub observed: Option<ObservedUsage>,
    /// Observation history, oldest first (readings outside the period are ignored).
    pub history: Vec<ObservationSample>,
    /// Usage per tier over the current period.
    pub period_rows: Vec<TierUsage>,
    /// Usage per tier over the current window.
    pub window_rows: Vec<TierUsage>,
    /// Usage per tier and minute over the current period (rate learning).
    pub minute_rows: Vec<(DateTime<Utc>, TierUsage)>,
    /// Usage per tier recorded at or after `observed.observed_at` up to the
    /// period end; empty without an observation.
    pub since_observation_rows: Vec<TierUsage>,
}

impl Ledger {
    /// Build one provider's ledger from plain rows: no I/O, cannot fail.
    ///
    /// `clock` places the period and window (see [`resolve_clock`]); the
    /// rows in `source` must have been read for those ranges. Learns the
    /// exchange rates from the observation history ([`learn_rate`]: the
    /// effective budgets are the learned ones when available), applies the
    /// per-model cost weights and shares, and folds in the latest
    /// observation ([`apply_observed`]).
    pub fn build(
        cfg: &BudgetConfig,
        provider: Provider,
        now: DateTime<Utc>,
        clock: &PeriodClock,
        source: LedgerSource,
    ) -> Ledger {
        let budget = cfg.provider(provider);
        let period = clock.current_period(now);
        let window = clock.current_window(now);
        let LedgerSource { observed, history, period_rows, window_rows, minute_rows, since_observation_rows } = source;
        let period_rows: Vec<TierUsage> = period_rows.into_iter().filter(|r| r.tier.provider() == provider).collect();
        let window_rows: Vec<TierUsage> = window_rows.into_iter().filter(|r| r.tier.provider() == provider).collect();

        let history: Vec<ObservationSample> = history.into_iter().filter(|s| period.contains(s.at) && s.at <= now).collect();
        let series = SpendSeries::new(cfg, provider, &minute_rows);
        let measured = |from, to| series.between(from, to);
        let learned = LearnedRate {
            period: learn_rate(&history, |s| s.period_used, None, measured),
            window: learn_rate(&history, |s| s.window_used, Some(WINDOW_LEARN_SPAN), measured),
        };
        let configured_period_budget = budget.period_weighted_tokens as f64;
        let configured_window_budget = budget.window_weighted_tokens as f64;
        let period_budget = learned.period.map(|r| r.budget).unwrap_or(configured_period_budget);
        let window_budget = learned.window.map(|r| r.budget).unwrap_or(configured_window_budget);

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

        let spent_since_observation = match &observed {
            Some(obs) if period.end > obs.observed_at => weighted_sum(cfg, provider, &since_observation_rows),
            _ => 0.0,
        };
        let mut ledger = Ledger {
            provider,
            now,
            period,
            window,
            total_period_weighted: tiers.iter().map(|t| t.period_weighted).sum(),
            total_window_weighted: tiers.iter().map(|t| t.window_weighted).sum(),
            tiers,
            period_budget,
            window_budget,
            configured_period_budget,
            configured_window_budget,
            window_enabled: clock.has_window(),
            learned,
            observed: None,
            spent_since_observation,
            anchor_source: clock.anchor_source,
        };
        if let Some(obs) = &observed {
            apply_observed(&mut ledger, obs);
        }
        ledger
    }

    /// An empty ledger with the given clock (tests and simulations): no
    /// tiers, nothing spent, zero budgets, window enabled, nothing learned.
    pub fn blank(provider: Provider, now: DateTime<Utc>, period: Period, window: Period) -> Ledger {
        Ledger {
            provider,
            now,
            period,
            window,
            tiers: Vec::new(),
            total_period_weighted: 0.0,
            total_window_weighted: 0.0,
            period_budget: 0.0,
            window_budget: 0.0,
            configured_period_budget: 0.0,
            configured_window_budget: 0.0,
            window_enabled: true,
            learned: LearnedRate::default(),
            observed: None,
            spent_since_observation: 0.0,
            anchor_source: AnchorSource::Config,
        }
    }

    /// The entry for a model (zeroed when the model has no entry).
    pub fn tier(&self, tier: &ModelTier) -> TierLedger {
        self.tiers.iter().find(|t| t.tier == *tier).cloned().unwrap_or(TierLedger { tier: tier.clone(), ..Default::default() })
    }

    /// Count a cost against this ledger as if it had been spent now (the
    /// daemon reserves predicted costs so several launches in one tick do
    /// not each think they are the only one).
    pub fn add_spend(&mut self, tier: &ModelTier, weighted_cost: f64) {
        if let Some(t) = self.tiers.iter_mut().find(|t| t.tier == *tier) {
            t.period_weighted += weighted_cost;
            t.window_weighted += weighted_cost;
        }
        self.total_period_weighted += weighted_cost;
        self.total_window_weighted += weighted_cost;
        self.spent_since_observation += weighted_cost;
    }

    /// Period spend as a fraction of the effective budget, from our usage
    /// rows alone. A zero budget counts as fully spent.
    pub fn measured_period_fraction(&self) -> f64 {
        if self.period_budget <= 0.0 { 1.0 } else { (self.total_period_weighted / self.period_budget).max(0.0) }
    }

    /// The observation this period's fractions start from, if there is one
    /// taken inside the period.
    fn observation_in_period(&self) -> Option<&ObservedUsage> {
        self.observed.as_ref().filter(|o| self.period.contains(o.observed_at))
    }

    /// Period spend as a fraction of the allowance.
    ///
    /// With an observation from this period that carries `period_used`, that
    /// percentage is the truth and only what we measured after it is added,
    /// converted at the effective budget. Without one, our own measurement
    /// against the effective budget. Clamped to `[0, 2]`.
    pub fn period_fraction(&self) -> f64 {
        let f = match self.observation_in_period().and_then(|o| o.period_used) {
            Some(used) => used + self.spent_since_observation / self.period_budget.max(1.0),
            None => self.measured_period_fraction(),
        };
        f.clamp(0.0, 2.0)
    }

    /// Where `period_fraction` comes from, for displays: `observed` or `measured`.
    pub fn period_fraction_source(&self) -> &'static str {
        if self.observation_in_period().and_then(|o| o.period_used).is_some() { "observed" } else { "measured" }
    }

    /// The latest observation's window reading while it is still current:
    /// its reset is ahead, or (no reset known) it is younger than
    /// [`OBSERVED_WINDOW_TTL`]. A reading from a window that has already
    /// rolled over must not keep blocking launches when no session is
    /// around to refresh it.
    pub fn observed_window_used(&self) -> Option<f64> {
        let obs = self.observed.as_ref()?;
        let current = match obs.window_resets_at {
            Some(reset) => reset > self.now,
            None => self.now - obs.observed_at < OBSERVED_WINDOW_TTL,
        };
        if current { obs.window_used } else { None }
    }

    /// Whether a rolling window applies: configured (`window_hours > 0`)
    /// or reported by a current observation ([`Self::observed_window_used`]).
    pub fn has_window(&self) -> bool {
        self.window_enabled || self.observed_window_used().is_some()
    }

    /// What a cost is divided by to express it as a window fraction: the
    /// effective window budget, or infinity when none is configured or
    /// learned (the window is then judged on the observation alone).
    pub fn window_divisor(&self) -> f64 {
        if self.window_budget > 0.0 { self.window_budget } else { f64::INFINITY }
    }

    /// Window spend as a fraction of the window allowance; 0 without a
    /// window ([`Self::has_window`]). With an observation that carries
    /// `window_used`, that plus what we measured since (at the effective
    /// window budget); else our rolling-window sum against the effective
    /// window budget.
    pub fn window_fraction(&self) -> f64 {
        if !self.has_window() {
            return 0.0;
        }
        match self.observed_window_used() {
            Some(used) => (used + self.spent_since_observation / self.window_divisor()).clamp(0.0, 2.0),
            None if self.window_budget <= 0.0 => 1.0,
            None => self.total_window_weighted / self.window_budget,
        }
    }

    /// Where `window_fraction` comes from, for displays.
    pub fn window_fraction_source(&self) -> &'static str {
        if self.observed_window_used().is_some() { "observed" } else { "measured" }
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
    use crate::budget::io::save_observed;
    use crate::domain::{Task, TaskId, TaskSource, UsageRecord};
    use crate::store::Store;

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
        assert!(ledger.learned.period.is_none() && ledger.learned.window.is_none());
        assert!(ledger.observed.is_none());
        assert_eq!(ledger.configured_period_budget, 80_000_000.0);
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

    fn observe(store: &Store, at: DateTime<Utc>, period_used: f64, window_used: Option<f64>) {
        let obs = ObservedUsage { period_used: Some(period_used), window_used, ..ObservedUsage::empty(at) };
        save_observed(store, Provider::Claude, &obs).unwrap();
    }

    #[test]
    fn an_observation_is_the_truth_and_only_later_usage_is_added() {
        let (store, task) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let cfg = cfg();
        // 8M weighted before the reading, 4M after it.
        usage(&store, task, "m1", ModelTier::sonnet(), 1_600_000, now - Duration::hours(2));
        observe(&store, now - Duration::hours(1), 0.35, None);
        usage(&store, task, "m2", ModelTier::sonnet(), 800_000, now - Duration::minutes(30));
        let ledger = Ledger::load(&store, &cfg, Provider::Claude, now).unwrap();
        assert!((ledger.measured_period_fraction() - 0.15).abs() < 1e-9, "12M of 80M");
        assert!((ledger.spent_since_observation - 4_000_000.0).abs() < 1e-6);
        // 35% reported + 4M / 80M (configured: one reading cannot teach a rate).
        assert!((ledger.period_fraction() - 0.40).abs() < 1e-9);
        assert_eq!(ledger.period_fraction_source(), "observed");
        assert!(ledger.learned.period.is_none());
        assert_eq!(ledger.period_budget, 80_000_000.0);
    }

    #[test]
    fn the_rate_is_learned_from_two_readings_and_what_was_measured_between() {
        let (store, task) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let cfg = cfg();
        observe(&store, now - Duration::hours(3), 0.30, Some(0.10));
        // 10M weighted between the readings; the provider moved 5 points.
        usage(&store, task, "m1", ModelTier::sonnet(), 2_000_000, now - Duration::hours(2));
        observe(&store, now - Duration::hours(1), 0.35, Some(0.15));
        // 1M after the last reading.
        usage(&store, task, "m2", ModelTier::sonnet(), 200_000, now - Duration::minutes(10));
        let ledger = Ledger::load(&store, &cfg, Provider::Claude, now).unwrap();
        let rate = ledger.learned.period.expect("rate learned");
        assert!((rate.budget - 200_000_000.0).abs() < 1.0, "10M per 5 points = 200M per period, got {}", rate.budget);
        assert!((rate.observed_delta - 0.05).abs() < 1e-9);
        assert!((rate.measured_delta - 10_000_000.0).abs() < 1e-6);
        assert_eq!((rate.from, rate.to, rate.samples), (now - Duration::hours(3), now - Duration::hours(1), 2));
        assert_eq!(ledger.period_budget, rate.budget);
        assert_eq!(ledger.configured_period_budget, 80_000_000.0);
        // 35% + 1M / 200M.
        assert!((ledger.period_fraction() - 0.355).abs() < 1e-9);
        // Shares follow the learned budget: fable 25% of 200M.
        assert!((ledger.tier(&ModelTier::fable()).period_budget - 50_000_000.0).abs() < 1e-6);
        // The window rate too: same 10M for 5 window points, readings 2h apart
        // are farther than WINDOW_LEARN_SPAN, so nothing is learned for it.
        assert!(ledger.learned.window.is_none());
        assert_eq!(ledger.window_budget, 12_000_000.0);
        assert!((ledger.window_fraction() - (0.15 + 1_000_000.0 / 12_000_000.0)).abs() < 1e-9);
    }

    #[test]
    fn window_rate_uses_readings_close_together() {
        let (store, task) = store_with_task();
        let now = at("2026-10-01T12:00:00Z");
        let cfg = cfg();
        observe(&store, now - Duration::minutes(50), 0.30, Some(0.10));
        usage(&store, task, "m1", ModelTier::sonnet(), 400_000, now - Duration::minutes(30)); // 2M
        observe(&store, now - Duration::minutes(5), 0.31, Some(0.14));
        let ledger = Ledger::load(&store, &cfg, Provider::Claude, now).unwrap();
        let w = ledger.learned.window.expect("window rate learned");
        assert!((w.budget - 50_000_000.0).abs() < 1.0, "2M per 4 points = 50M per window");
        assert!(ledger.learned.period.is_none(), "1 point is below MIN_OBSERVED_DELTA");
        assert!((ledger.window_budget - 50_000_000.0).abs() < 1.0);
    }

    #[test]
    fn learning_ignores_tiny_measurements_resets_and_other_periods() {
        let now = at("2026-10-01T12:00:00Z");
        let s = |h: i64, p: f64| ObservationSample { at: now - Duration::hours(h), period_used: Some(p), window_used: None };
        // A jump of 20 points with only 1k tokens measured: outside usage, no rate.
        let r = learn_rate(&[s(3, 0.1), s(1, 0.3)], |x| x.period_used, None, |_, _| 1_000.0);
        assert!(r.is_none());
        // Enough tokens: the pair with the largest budget wins (the 0.5 reading
        // before the reset never pairs with later ones: its delta is negative).
        let r = learn_rate(&[s(5, 0.5), s(3, 0.1), s(2, 0.11), s(1, 0.3)], |x| x.period_used, None, |_, _| 4_000_000.0).unwrap();
        assert_eq!((r.from, r.to), (now - Duration::hours(2), now - Duration::hours(1)), "4M / 0.19 beats 4M / 0.20");
        assert!((r.budget - 4_000_000.0 / 0.19).abs() < 1.0);
        assert_eq!(r.samples, 2);
        // Usage outside powerqueue contaminates a span: the clean span wins.
        let m = |from: DateTime<Utc>, to: DateTime<Utc>| -> f64 {
            // 4M between 5h and 3h ago, 1M between 3h and 1h ago.
            let clean = if from <= now - Duration::hours(5) && to >= now - Duration::hours(3) { 4_000_000.0 } else { 0.0 };
            let dirty = if from <= now - Duration::hours(3) && to >= now - Duration::hours(1) { 1_000_000.0 } else { 0.0 };
            clean + dirty
        };
        let r = learn_rate(&[s(5, 0.10), s(3, 0.20), s(1, 0.30)], |x| x.period_used, None, m).unwrap();
        assert_eq!((r.from, r.to), (now - Duration::hours(5), now - Duration::hours(3)));
        assert!((r.budget - 40_000_000.0).abs() < 1.0, "the 10 points moved by someone else's 1M do not win: {}", r.budget);
        // Nothing with a value: nothing learned.
        assert!(learn_rate(&[], |x| x.period_used, None, |_, _| 1e9).is_none());
        assert!(learn_rate(&[s(1, 0.3)], |x| x.window_used, None, |_, _| 1e9).is_none());
        // A reset inside the readings: pairs never span the decrease, so the
        // tokens burned before it cannot inflate the budget (0.02 → 0.60,
        // reset, 0.05 → 0.10: pairs on either side are candidates, the one
        // across the reset never is).
        let s2 = |m: i64, p: f64| ObservationSample { at: now - Duration::minutes(m), period_used: None, window_used: Some(p) };
        let readings = [s2(55, 0.02), s2(35, 0.60), s2(10, 0.05), s2(0, 0.10)];
        let measured = |from: DateTime<Utc>, to: DateTime<Utc>| {
            let before = if from <= now - Duration::minutes(35) { 60_000_000.0 } else { 0.0 };
            let after = if to >= now { 2_000_000.0 } else { 0.0 };
            before + after
        };
        let r = learn_rate(&readings, |x| x.window_used, Some(Duration::hours(1)), measured).unwrap();
        assert_eq!((r.from, r.to), (now - Duration::minutes(55), now - Duration::minutes(35)), "a pair on one side of the reset");
        assert!((r.budget - 60_000_000.0 / 0.58).abs() < 1.0, "{}", r.budget);
        assert!(r.budget < 500_000_000.0, "the cross-reset pair (62M for 8 points = 775M) must never be a candidate");
        // max_span keeps only pairs close together.
        let r = learn_rate(&[s(5, 0.0), s(1, 0.3)], |x| x.period_used, Some(Duration::hours(2)), |_, _| 1e9);
        assert!(r.is_none(), "the only pair is 4h apart");
    }

    #[test]
    fn a_window_reading_expires_with_its_reset_or_after_five_hours() {
        let now = at("2026-10-01T12:00:00Z");
        let period = Period { start: now - Duration::days(1), end: now + Duration::days(6) };
        let mut l = Ledger::blank(Provider::Claude, now, period, Period { start: now - Duration::hours(5), end: now });
        l.window_enabled = false;
        l.window_budget = 100.0;
        l.observed = Some(ObservedUsage {
            window_used: Some(0.9),
            window_resets_at: Some(now + Duration::minutes(5)),
            ..ObservedUsage::empty(now - Duration::hours(4))
        });
        assert!(l.has_window() && (l.window_fraction() - 0.9).abs() < 1e-9);
        l.observed.as_mut().unwrap().window_resets_at = Some(now - Duration::minutes(1));
        assert!(!l.has_window(), "the window it described has rolled over");
        assert_eq!(l.window_fraction(), 0.0);
        assert_eq!(l.window_fraction_source(), "measured");
        l.observed.as_mut().unwrap().window_resets_at = None;
        assert!(l.has_window(), "no reset known: trusted for five hours");
        l.observed.as_mut().unwrap().observed_at = now - Duration::hours(6);
        assert!(!l.has_window());
        // No budget at all: the observation alone is judged, costs convert to nothing.
        l.observed.as_mut().unwrap().observed_at = now;
        l.window_budget = 0.0;
        l.spent_since_observation = 1e9;
        assert_eq!(l.window_divisor(), f64::INFINITY);
        assert!((l.window_fraction() - 0.9).abs() < 1e-9);
    }

    #[test]
    fn spend_series_sums_minutes_between_instants() {
        let (store, task) = store_with_task();
        let cfg = cfg();
        let t = at("2026-10-01T12:00:30Z");
        usage(&store, task, "m1", ModelTier::sonnet(), 100, t); // 500 weighted, minute 12:00
        usage(&store, task, "m2", ModelTier::fable(), 100, t + Duration::minutes(1)); // 2500, minute 12:01
        usage(&store, task, "m3", ModelTier::new("gpt-6.1-sol"), 100, t + Duration::minutes(2)); // not claude
        let rows = store.usage_by_minute(t - Duration::hours(1), t + Duration::hours(1)).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, at("2026-10-01T12:00:00Z"));
        let series = SpendSeries::new(&cfg, Provider::Claude, &rows);
        assert_eq!(series.until(at("2026-10-01T12:00:00Z")), 0.0);
        assert_eq!(series.until(at("2026-10-01T12:00:45Z")), 500.0, "a minute counts once a reading is inside it");
        assert_eq!(series.until(at("2026-10-01T13:00:00Z")), 3000.0);
        assert_eq!(series.between(at("2026-10-01T12:00:45Z"), at("2026-10-01T12:01:30Z")), 2500.0);
        assert_eq!(series.between(at("2026-10-01T13:00:00Z"), at("2026-10-01T12:00:00Z")), 0.0);
    }

    #[test]
    fn observed_usage_moves_the_period() {
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
        assert!((ledger.period_fraction() - 0.5).abs() < 1e-9, "the reading is the truth; nothing measured after it");
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
    fn period_fraction_is_clamped_and_spend_is_reserved() {
        let now = at("2026-10-01T12:00:00Z");
        let period = Period { start: now - Duration::days(1), end: now + Duration::days(6) };
        let mut ledger = Ledger::blank(Provider::Claude, now, period, Period { start: now - Duration::hours(5), end: now });
        ledger.tiers.push(TierLedger { tier: ModelTier::opus(), period_budget: 50.0, ..Default::default() });
        ledger.total_period_weighted = 10.0;
        ledger.period_budget = 100.0;
        ledger.window_budget = 100.0;
        assert!((ledger.period_fraction() - 0.1).abs() < 1e-9);
        ledger.observed = Some(ObservedUsage { period_used: Some(5.0), ..ObservedUsage::empty(now) });
        assert_eq!(ledger.period_fraction(), 2.0, "a nonsense reading is clamped");
        ledger.observed = None;
        ledger.period_budget = 0.0;
        assert_eq!(ledger.period_fraction(), 1.0, "zero budget counts as spent");
        ledger.period_budget = 100.0;
        ledger.add_spend(&ModelTier::opus(), 30.0);
        assert_eq!(ledger.tier(&ModelTier::opus()).period_weighted, 30.0);
        assert_eq!(ledger.total_period_weighted, 40.0);
        assert_eq!(ledger.total_window_weighted, 30.0);
        assert_eq!(ledger.spent_since_observation, 30.0);
        assert_eq!(ledger.period_fraction_source(), "measured");
        assert_eq!(ledger.window_fraction_source(), "measured");
        assert_eq!(tier_weight(&BudgetConfig::default(), &ModelTier::new("codex:custom")), 1.0);
        assert_eq!(tier_share(&BudgetConfig::default(), &ModelTier::new("codex:custom")), 0.0);
        assert_eq!(tier_weight(&BudgetConfig::default(), &ModelTier::fable()), 5.0);
        assert_eq!(tier_share(&BudgetConfig::default(), &ModelTier::fable()), 0.25);
    }
}

#[cfg(test)]
mod properties {
    use super::*;
    use crate::budget::period::PeriodClock;
    use crate::domain::TokenUsage;
    use crate::scheduler::reserve;
    use crate::strategies::{known_models, ledgers, model, origin};
    use proptest::collection::vec;
    use proptest::prelude::*;

    fn token_usage() -> impl Strategy<Value = TokenUsage> {
        (0u64..1_000_000, 0u64..1_000_000, 0u64..1_000_000, 0u64..1_000_000).prop_map(
            |(input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens)| TokenUsage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
            },
        )
    }

    /// Usage rows as the store returns them: one row per tier.
    fn tier_rows() -> impl Strategy<Value = Vec<TierUsage>> {
        vec((model(), token_usage(), 0u64..50), 0..6).prop_map(|rows| {
            let mut out: Vec<TierUsage> = Vec::new();
            for (tier, usage, messages) in rows {
                match out.iter_mut().find(|r| r.tier == tier) {
                    Some(r) => {
                        r.usage.add(&usage);
                        r.messages += messages;
                    }
                    None => out.push(TierUsage { tier, usage, messages }),
                }
            }
            out
        })
    }

    /// `rows` with `extra` folded in, keeping one row per tier.
    fn merged(rows: &[TierUsage], extra: &[TierUsage]) -> Vec<TierUsage> {
        let mut out = rows.to_vec();
        for e in extra {
            match out.iter_mut().find(|r| r.tier == e.tier) {
                Some(r) => {
                    r.usage.add(&e.usage);
                    r.messages += e.messages;
                }
                None => out.push(e.clone()),
            }
        }
        out
    }

    fn cfg() -> BudgetConfig {
        let mut cfg = BudgetConfig::default();
        for p in Provider::ALL {
            cfg.providers.get_mut(p).period_anchor = Some("2026-09-28T00:00:00Z".into());
        }
        cfg
    }

    fn build(cfg: &BudgetConfig, provider: Provider, period_rows: Vec<TierUsage>, window_rows: Vec<TierUsage>) -> Ledger {
        let clock = PeriodClock::from_provider(cfg.provider(provider), origin());
        let source = LedgerSource { period_rows, window_rows, ..LedgerSource::default() };
        Ledger::build(cfg, provider, origin(), &clock, source)
    }

    fn weighted_sum_of(cfg: &BudgetConfig, provider: Provider, rows: &[TierUsage]) -> f64 {
        rows.iter().filter(|r| r.tier.provider() == provider).map(|r| r.usage.weighted() * tier_weight(cfg, &r.tier)).sum()
    }

    proptest! {
        /// A reservation lands on exactly one provider's ledger (the model's),
        /// raises its period and window totals and the tier's own counters by
        /// exactly the cost, and leaves every other ledger untouched. A model
        /// whose provider has no ledger changes nothing.
        #[test]
        fn reserve_charges_exactly_one_provider(before in ledgers(), tier in model(), cost in 0.0f64..1.0e8) {
            let mut after = before.clone();
            reserve(&mut after, &tier, cost);
            let provider = tier.provider();
            for (p, l) in before.iter() {
                let changed = after.get(p).expect("same providers");
                if p == provider {
                    prop_assert!((changed.total_period_weighted - (l.total_period_weighted + cost)).abs() < 1e-6);
                    prop_assert!((changed.total_window_weighted - (l.total_window_weighted + cost)).abs() < 1e-6);
                    prop_assert!((changed.spent_since_observation - (l.spent_since_observation + cost)).abs() < 1e-6);
                    prop_assert!((changed.tier(&tier).period_weighted - (l.tier(&tier).period_weighted + cost)).abs() < 1e-6);
                    prop_assert!((changed.tier(&tier).window_weighted - (l.tier(&tier).window_weighted + cost)).abs() < 1e-6);
                    prop_assert!(changed.total_period_weighted >= 0.0 && changed.total_window_weighted >= 0.0);
                    prop_assert!(changed.tiers.iter().all(|t| t.period_weighted >= 0.0 && t.window_weighted >= 0.0));
                    // Only the charged tier moved.
                    for t in &l.tiers {
                        if t.tier != tier {
                            prop_assert_eq!(&changed.tier(&t.tier), t);
                        }
                    }
                } else {
                    prop_assert_eq!(changed, l, "{} must not change for a {} model", p, provider);
                }
            }
            prop_assert_eq!(after.by_provider.len(), before.by_provider.len());
            if before.get(provider).is_none() {
                prop_assert_eq!(after, before, "no ledger for {}: nothing to charge", provider);
            }
        }

        /// Every shipped model has a positive cost weight; unknown models cost
        /// as much as Sonnet (1.0) and have no share.
        #[test]
        fn weights_are_positive(name in "[a-z]{3,8}") {
            let cfg = BudgetConfig::default();
            for m in known_models() {
                prop_assert!(tier_weight(&cfg, &m) > 0.0, "{}", m);
                prop_assert!(tier_share(&cfg, &m) >= 0.0, "{}", m);
            }
            let unknown = ModelTier::new(&format!("codex:{name}"));
            prop_assert_eq!(tier_weight(&cfg, &unknown), 1.0);
            prop_assert_eq!(tier_share(&cfg, &unknown), 0.0);
        }

        /// The ledger's totals are the tier-weighted sum of the provider's
        /// rows, rows of other providers are ignored, and adding usage never
        /// lowers a spend fraction. Without an observation the fractions are
        /// `measured`; `period_fraction` is clamped to `[0, 2]`, the measured
        /// window fraction is only bounded below (the policy compares it
        /// against the margin, so a value above 2 is as blocking as 2).
        #[test]
        fn build_is_monotone_in_usage(
            provider in prop::sample::select(Provider::ALL.to_vec()),
            period_rows in tier_rows(),
            window_rows in tier_rows(),
            more_period in tier_rows(),
            more_window in tier_rows(),
        ) {
            let cfg = cfg();
            let base = build(&cfg, provider, period_rows.clone(), window_rows.clone());
            prop_assert_eq!(base.provider, provider);
            prop_assert!(base.period.contains(origin()));
            let expected = weighted_sum_of(&cfg, provider, &period_rows);
            prop_assert!((base.total_period_weighted - expected).abs() < 1e-6 * expected.max(1.0), "{} vs {}", base.total_period_weighted, expected);
            let expected_window = weighted_sum_of(&cfg, provider, &window_rows);
            prop_assert!((base.total_window_weighted - expected_window).abs() < 1e-6 * expected_window.max(1.0));
            prop_assert!(base.tiers.iter().all(|t| t.tier.provider() == provider), "{:?}", base.tiers);
            for m in cfg.models_for(provider) {
                prop_assert!(base.tiers.iter().any(|t| t.tier == m), "configured model {} always has a tier", m);
            }
            prop_assert_eq!(base.period_fraction_source(), "measured");
            prop_assert_eq!(base.window_fraction_source(), "measured");
            prop_assert!((0.0..=2.0).contains(&base.period_fraction()));
            prop_assert!(base.window_fraction() >= 0.0);
            prop_assert_eq!(base.period_budget, base.configured_period_budget, "nothing learned from an empty history");

            let more = build(&cfg, provider, merged(&period_rows, &more_period), merged(&window_rows, &more_window));
            prop_assert!(more.total_period_weighted >= base.total_period_weighted - 1e-6);
            prop_assert!(more.total_window_weighted >= base.total_window_weighted - 1e-6);
            prop_assert!(more.period_fraction() >= base.period_fraction() - 1e-9, "{} < {}", more.period_fraction(), base.period_fraction());
            prop_assert!(more.window_fraction() >= base.window_fraction() - 1e-9, "{} < {}", more.window_fraction(), base.window_fraction());
            prop_assert_eq!(more.period, base.period);
            prop_assert_eq!(more.window, base.window);

            // Rows of other providers never show up.
            let own: Vec<TierUsage> = period_rows.iter().filter(|r| r.tier.provider() == provider).cloned().collect();
            let foreign: Vec<TierUsage> = period_rows.iter().filter(|r| r.tier.provider() != provider).cloned().collect();
            let without = build(&cfg, provider, own.clone(), Vec::new());
            let with = build(&cfg, provider, merged(&own, &foreign), Vec::new());
            prop_assert_eq!(with, without);
        }
    }
}
