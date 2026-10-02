//! The decision: which model may a task use *now*?
//!
//! This version decides over the Claude provider only (the multi-provider
//! candidate walk lands on the budget-engine branch); the data model is
//! already per provider: ledgers come from [`Ledgers`], capability order and
//! downgrades from `BudgetConfig::downgrade_chain`.
//!
//! Rules, in order:
//! 1. Hard overrides (`task.model_override`, PRIORITY.md `## Models`) pick a
//!    preferred model; the policy may still downgrade when that model is out
//!    of budget, and says so in `reasons`. It never silently upgrades above a
//!    user-chosen model.
//! 2. A model that reported `rate_limit` is unavailable until its cooldown ends.
//! 3. The rolling window must have room for the predicted cost.
//! 4. A model is *eligible* for a task when `task.criticality <= min_criticality`,
//!    or when it is relaxed: after `relax_after_fraction` of the period, if the
//!    model's spend fraction is below the period's elapsed fraction (it is
//!    under-paced), one criticality level lower qualifies; in the end game
//!    (`endgame_fraction`) two levels lower qualify as long as the predicted
//!    cost fits the model's remaining budget with the safety margin.
//! 5. Among eligible models prefer the preferred model, else the most capable
//!    for critical/high work, the configured default for normal work and the
//!    configured low model for low work.
//! 6. If nothing is eligible, the task is throttled until the earliest instant
//!    at which something could change: the window roll-over (approximated as
//!    15 minutes, because we do not track when the oldest window usage expires),
//!    a rate-limit expiry, the next relaxation boundary, or the period end.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::BudgetConfig;
use crate::domain::{Criticality, ModelTier, Provider, Task};

use super::estimator::Prediction;
use super::ledger::{Ledger, Ledgers, tier_share, tier_weight};

/// kv key under which [`RateLimitState`] is persisted.
pub const RATE_LIMITS_KEY: &str = "budget.rate_limits";

/// How soon to look again when the rolling window is the blocker. The ledger
/// only knows the window total, not when its oldest row falls out, so this is
/// a cheap approximation of the roll-over; re-evaluation is inexpensive.
pub const WINDOW_RECHECK: Duration = Duration::minutes(15);

/// Per-model cooldown after a rate-limit error, persisted in kv `budget.rate_limits`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitState {
    pub exhausted_until: std::collections::BTreeMap<ModelTier, DateTime<Utc>>,
}

impl RateLimitState {
    pub fn is_exhausted(&self, tier: &ModelTier, now: DateTime<Utc>) -> bool {
        self.exhausted_until.get(tier).map(|t| *t > now).unwrap_or(false)
    }
    pub fn mark(&mut self, tier: ModelTier, until: DateTime<Utc>) {
        self.exhausted_until.insert(tier, until);
    }
    /// Mark every configured model of `provider` exhausted until `until`
    /// (subscription limits are account-wide).
    pub fn mark_provider(&mut self, cfg: &BudgetConfig, provider: Provider, until: DateTime<Utc>) {
        for tier in cfg.models_for(provider) {
            self.mark(tier, until);
        }
    }
    /// Forget the cooldowns of every model of `provider`.
    pub fn clear_provider(&mut self, provider: Provider) {
        self.exhausted_until.retain(|t, _| t.provider() != provider);
    }
    pub fn clear_expired(&mut self, now: DateTime<Utc>) {
        self.exhausted_until.retain(|_, t| *t > now);
    }
    /// Cooldown end for a model, if it is currently exhausted.
    pub fn until(&self, tier: &ModelTier, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.exhausted_until.get(tier).copied().filter(|t| *t > now)
    }
    /// The soonest cooldown end among `provider`'s models, if any is exhausted.
    pub fn provider_until(&self, provider: Provider, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.exhausted_until.iter().filter(|(t, until)| t.provider() == provider && **until > now).map(|(_, u)| *u).min()
    }
    pub fn is_empty(&self) -> bool {
        self.exhausted_until.is_empty()
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

/// One model's verdict for one task.
#[derive(Debug, Clone, PartialEq)]
struct Verdict {
    eligible: bool,
    reason: String,
    /// When the blocker might lift (only for ineligible models).
    retry_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct Policy<'a> {
    pub cfg: &'a BudgetConfig,
    pub ledgers: &'a Ledgers,
    pub rate_limits: &'a RateLimitState,
}

impl<'a> Policy<'a> {
    pub fn new(cfg: &'a BudgetConfig, ledgers: &'a Ledgers, rate_limits: &'a RateLimitState) -> Self {
        Self { cfg, ledgers, rate_limits }
    }

    /// The Claude ledger, which this version decides over.
    fn ledger(&self) -> Option<&'a Ledger> {
        self.ledgers.get(Provider::Claude)
    }

    /// Models that can ever be chosen: enabled with a positive share on an
    /// enabled provider, most capable first.
    pub fn candidates(&self) -> Vec<ModelTier> {
        if self.ledger().is_none() {
            return Vec::new();
        }
        self.cfg.models_for(Provider::Claude).into_iter().filter(|t| tier_share(self.cfg, t) > 0.0).collect()
    }

    /// Decide for `task`, given its predicted cost and a preferred model.
    /// `task.model_override` wins over `preferred` (which usually comes from
    /// `PRIORITY.md`). Pure: the same inputs always give the same decision.
    pub fn decide(&self, task: &Task, prediction: Prediction, preferred: Option<ModelTier>) -> Decision {
        let Some(ledger) = self.ledger() else {
            let now = self.ledgers.by_provider.values().next().map(|l| l.now).unwrap_or_else(Utc::now);
            return Decision {
                model: None,
                retry_at: Some(now + WINDOW_RECHECK),
                prediction,
                reasons: vec!["claude provider is disabled (budget.providers.claude.enabled = false)".to_string()],
            };
        };
        let now = ledger.now;
        let mut reasons = Vec::new();
        reasons.push(format!(
            "period {:.0}% elapsed, {:.0}% spent; window {:.0}% used; predicted cost {:.0} weighted tokens ({})",
            ledger.elapsed_fraction() * 100.0,
            ledger.period_fraction() * 100.0,
            ledger.window_fraction() * 100.0,
            prediction.weighted_tokens,
            prediction.basis
        ));

        let candidates = self.candidates();
        let mut eligible = Vec::new();
        let mut retries = Vec::new();
        for tier in &candidates {
            let v = self.verdict(task, tier, prediction.weighted_tokens);
            reasons.push(format!("{tier}: {}", v.reason));
            if v.eligible {
                eligible.push(tier.clone());
            } else if let Some(at) = v.retry_at {
                retries.push(at);
            }
        }

        let preferred = match (task.model_override.clone(), preferred) {
            (Some(t), _) => {
                reasons.push(format!("model override: {t}"));
                Some(t)
            }
            (None, Some(t)) => {
                reasons.push(format!("preferred by rules: {t}"));
                Some(t)
            }
            (None, None) => None,
        };

        let model = match preferred {
            Some(p) => self.pick_at_or_below(&eligible, &p, &mut reasons, &format!("preferred {p}")),
            None => match task.criticality {
                Criticality::Critical | Criticality::High => {
                    let best = eligible.first().cloned();
                    if let Some(t) = &best {
                        reasons.push(format!("{} task: most capable eligible tier is {t}", task.criticality));
                    }
                    best
                }
                Criticality::Normal => self.pick_at_or_below(
                    &eligible,
                    &self.cfg.default_model,
                    &mut reasons,
                    &format!("default model {}", self.cfg.default_model),
                ),
                Criticality::Low => self.pick_at_or_below(
                    &eligible,
                    &self.cfg.low_model,
                    &mut reasons,
                    &format!("low model {}", self.cfg.low_model),
                ),
            },
        };

        let retry_at = if model.is_some() {
            None
        } else {
            let period_end = ledger.period.end;
            let mut at = retries.into_iter().min().unwrap_or(period_end).min(period_end);
            if at <= now {
                at = now + WINDOW_RECHECK;
            }
            reasons.push(format!("nothing eligible; retry at {}", at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
            Some(at)
        };

        Decision { model, retry_at, prediction, reasons }
    }

    /// Is `tier` eligible for this task right now, and why/why not?
    pub fn eligibility(&self, task: &Task, tier: &ModelTier, predicted_weighted: f64) -> (bool, String) {
        let v = self.verdict(task, tier, predicted_weighted);
        (v.eligible, v.reason)
    }

    /// The preferred model if eligible, else the most capable eligible model
    /// below it in its provider's downgrade chain.
    fn pick_at_or_below(
        &self,
        eligible: &[ModelTier],
        preferred: &ModelTier,
        reasons: &mut Vec<String>,
        label: &str,
    ) -> Option<ModelTier> {
        if eligible.contains(preferred) {
            reasons.push(format!("{label} is eligible"));
            return Some(preferred.clone());
        }
        let fallback = self.cfg.downgrade_chain(preferred).into_iter().skip(1).find(|t| eligible.contains(t));
        match &fallback {
            Some(t) => reasons.push(format!("{label} not eligible; downgraded to {t}")),
            None => reasons.push(format!("{label} not eligible and no cheaper tier is")),
        }
        fallback
    }

    fn verdict(&self, task: &Task, tier: &ModelTier, predicted_weighted: f64) -> Verdict {
        let blocked = |reason: String, retry_at: Option<DateTime<Utc>>| Verdict { eligible: false, reason, retry_at };
        let Some(ledger) = self.ledgers.for_model(tier) else {
            return blocked(format!("provider {} is disabled", tier.provider()), None);
        };
        let now = ledger.now;
        let period = ledger.period;

        let Some(model) = self.cfg.model_budget(tier).filter(|m| m.enabled && m.share > 0.0) else {
            return blocked("disabled or no budget share".to_string(), None);
        };
        if let Some(until) = self.rate_limits.until(tier, now) {
            return blocked(
                format!("rate limited until {}", until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                Some(until),
            );
        }

        // Criticality gate with relaxation.
        let e = ledger.elapsed_fraction();
        let tl = ledger.tier(tier);
        let s = tl.period_spent_fraction();
        let endgame = e >= self.cfg.endgame_fraction;
        let under_paced = e >= model.relax_after_fraction && s < e;
        let levels = if endgame {
            2
        } else if under_paced {
            1
        } else {
            0
        };
        let allowed = lower_by(model.min_criticality, levels);
        if task.criticality > allowed {
            let needed = level_index(task.criticality).saturating_sub(level_index(model.min_criticality));
            let relax_at = period.start + fraction_of(period.len(), model.relax_after_fraction);
            let endgame_at = period.start + fraction_of(period.len(), self.cfg.endgame_fraction);
            let boundary = if needed == 1 && !endgame && e < model.relax_after_fraction {
                relax_at
            } else if needed <= 2 && !endgame {
                endgame_at
            } else {
                period.end
            };
            let retry_at = if boundary > now { boundary } else { now + WINDOW_RECHECK };
            let detail = if levels > 0 {
                format!("reserved for {} (relaxed to {allowed}); task is {}", model.min_criticality, task.criticality)
            } else {
                format!("reserved for {}; task is {}", model.min_criticality, task.criticality)
            };
            let pacing = format!("{:.0}% of its budget spent at {:.0}% of the period", s * 100.0, e * 100.0);
            return blocked(format!("{detail} ({pacing})"), Some(retry_at));
        }

        // Budget gates.
        let cost = predicted_weighted * tier_weight(self.cfg, tier);
        let margin = (1.0 - self.cfg.safety_margin).max(0.0);
        let available = tl.period_remaining() * margin;
        if cost > available {
            return blocked(
                format!("period budget: needs {:.0} weighted tokens, {:.0} available after safety margin", cost, available),
                Some(period.end),
            );
        }
        let overall_after = ledger.period_fraction() + cost / ledger.period_budget.max(1.0);
        if overall_after > margin {
            return blocked(
                format!(
                    "overall period budget: {:.0}% spent (calibrated), this task would push it to {:.0}%",
                    ledger.period_fraction() * 100.0,
                    overall_after * 100.0
                ),
                Some(period.end),
            );
        }
        if ledger.window_enabled && ledger.total_window_weighted + cost > ledger.window_budget {
            return blocked(
                format!(
                    "window: {:.0} of {:.0} weighted tokens used, needs {:.0} more",
                    ledger.total_window_weighted, ledger.window_budget, cost
                ),
                Some(now + WINDOW_RECHECK),
            );
        }

        let how = if levels > 0 { format!("eligible (relaxed to {allowed})") } else { "eligible".to_string() };
        Verdict {
            eligible: true,
            reason: format!("{how}; {:.0}% of its budget spent at {:.0}% of the period", s * 100.0, e * 100.0),
            retry_at: None,
        }
    }
}

fn level_index(c: Criticality) -> usize {
    Criticality::ALL.iter().position(|x| *x == c).unwrap_or(0)
}

/// `levels` steps less important than `c`, saturating at `Low`.
fn lower_by(c: Criticality, levels: usize) -> Criticality {
    let idx = (level_index(c) + levels).min(Criticality::ALL.len() - 1);
    Criticality::ALL[idx]
}

fn fraction_of(d: Duration, f: f64) -> Duration {
    Duration::seconds((d.num_seconds() as f64 * f.clamp(0.0, 1.0)).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::ledger::{Calibration, TierLedger};
    use crate::budget::period::{AnchorSource, Period};
    use crate::domain::TaskSource;

    const PERIOD_HOURS: i64 = 24 * 7;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        at("2026-10-01T12:00:00Z")
    }

    fn fable() -> ModelTier {
        ModelTier::fable()
    }
    fn opus() -> ModelTier {
        ModelTier::opus()
    }
    fn sonnet() -> ModelTier {
        ModelTier::sonnet()
    }
    fn haiku() -> ModelTier {
        ModelTier::haiku()
    }

    /// A Claude ledger at `elapsed` of the period with each tier at the given spent fraction.
    fn ledger(cfg: &BudgetConfig, elapsed: f64, spent: &[(ModelTier, f64)]) -> Ledgers {
        let now = now();
        let start = now - fraction_of(Duration::hours(PERIOD_HOURS), elapsed);
        let period = Period { start, end: start + Duration::hours(PERIOD_HOURS) };
        let claude = &cfg.providers.claude;
        let tiers: Vec<TierLedger> = cfg
            .models_for(Provider::Claude)
            .into_iter()
            .map(|tier| {
                let budget = claude.period_weighted_tokens as f64 * tier_share(cfg, &tier);
                let frac = spent.iter().find(|(t, _)| *t == tier).map(|(_, f)| *f).unwrap_or(0.0);
                TierLedger { tier, period_weighted: budget * frac, period_budget: budget, ..Default::default() }
            })
            .collect();
        Ledgers::single(Ledger {
            provider: Provider::Claude,
            now,
            period,
            window: Period { start: now - Duration::hours(5), end: now },
            total_period_weighted: tiers.iter().map(|t| t.period_weighted).sum(),
            total_window_weighted: 0.0,
            tiers,
            period_budget: claude.period_weighted_tokens as f64,
            window_budget: claude.window_weighted_tokens as f64,
            window_enabled: true,
            calibration: None,
            observed: None,
            anchor_source: AnchorSource::Config,
        })
    }

    fn claude(l: &Ledgers) -> &Ledger {
        l.get(Provider::Claude).unwrap()
    }

    fn claude_mut(l: &mut Ledgers) -> &mut Ledger {
        l.get_mut(Provider::Claude).unwrap()
    }

    fn task(crit: Criticality) -> Task {
        let mut t = Task::new("ENG-1", "t", TaskSource::Manual);
        t.criticality = crit;
        t
    }

    fn prediction(tokens: f64) -> Prediction {
        Prediction { weighted_tokens: tokens, wall_secs: 600.0, confidence: 0.5, basis: "test".into() }
    }

    fn decide(
        cfg: &BudgetConfig,
        ledgers: &Ledgers,
        limits: &RateLimitState,
        t: &Task,
        preferred: Option<ModelTier>,
    ) -> Decision {
        Policy::new(cfg, ledgers, limits).decide(t, prediction(100_000.0), preferred)
    }

    #[test]
    fn fable_is_reserved_for_critical_early_in_the_period() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.1, &[]);
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), None);
        assert_eq!(d.model, Some(fable()));
        assert!(d.retry_at.is_none());
        let d = decide(&cfg, &l, &limits, &task(Criticality::High), None);
        assert_eq!(d.model, Some(opus()), "high gets the most capable tier it is allowed: opus");
        assert!(d.reasons.iter().any(|r| r.starts_with("fable: reserved for critical")), "{:?}", d.reasons);
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), None);
        assert_eq!(d.model, Some(sonnet()));
    }

    #[test]
    fn fable_relaxes_to_high_after_half_the_period_when_under_spent() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.6, &[(fable(), 0.2)]);
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::High), None);
        assert_eq!(d.model, Some(fable()));
        assert!(d.reasons.iter().any(|r| r.contains("fable: eligible (relaxed to high)")), "{:?}", d.reasons);
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), None);
        assert_eq!(d.model, Some(sonnet()), "one level of relaxation does not reach normal");
    }

    #[test]
    fn no_relaxation_when_tier_is_over_paced() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.6, &[(fable(), 0.7)]);
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::High), None);
        assert_eq!(d.model, Some(opus()));
        assert!(d.reasons.iter().any(|r| r.starts_with("fable: reserved for critical; task is high")), "{:?}", d.reasons);
    }

    #[test]
    fn end_game_opens_fable_to_normal_but_not_low() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.85, &[(fable(), 0.9)]);
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), None);
        // Normal prefers the default model (sonnet) even though fable is eligible.
        assert_eq!(d.model, Some(sonnet()));
        assert!(d.reasons.iter().any(|r| r.contains("fable: eligible (relaxed to normal)")), "{:?}", d.reasons);
        let (ok, _) = Policy::new(&cfg, &l, &limits).eligibility(&task(Criticality::Low), &fable(), 1.0);
        assert!(!ok, "two levels below critical is normal, not low");
        // With a preference for fable, normal work now gets it.
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), Some(fable()));
        assert_eq!(d.model, Some(fable()));
    }

    #[test]
    fn window_exhaustion_throttles_with_short_retry() {
        let cfg = BudgetConfig::default();
        let mut l = ledger(&cfg, 0.1, &[]);
        claude_mut(&mut l).total_window_weighted = claude(&l).window_budget - 10.0;
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::Critical), None);
        assert_eq!(d.model, None);
        assert_eq!(d.retry_at, Some(now() + WINDOW_RECHECK));
        assert!(d.reasons.iter().any(|r| r.starts_with("fable: window:")), "{:?}", d.reasons);
        assert!(d.reasons.iter().any(|r| r.starts_with("haiku: window:")), "{:?}", d.reasons);
        // Without a window the same spend is no blocker.
        claude_mut(&mut l).window_enabled = false;
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::Critical), None);
        assert_eq!(d.model, Some(fable()));
    }

    #[test]
    fn rate_limit_excludes_tier_until_expiry() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.1, &[]);
        let mut limits = RateLimitState::default();
        let until = now() + Duration::minutes(30);
        limits.mark(fable(), until);
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), None);
        assert_eq!(d.model, Some(opus()));
        assert!(d.reasons.iter().any(|r| r.starts_with("fable: rate limited until 2026-10-01T12:30:00Z")), "{:?}", d.reasons);
        limits.clear_expired(now() + Duration::hours(1));
        assert!(!limits.is_exhausted(&fable(), now() + Duration::hours(1)));
        assert!(limits.exhausted_until.is_empty());
    }

    #[test]
    fn provider_wide_marks_and_clears() {
        let cfg = BudgetConfig::default();
        let mut limits = RateLimitState::default();
        let until = now() + Duration::minutes(30);
        limits.mark_provider(&cfg, Provider::Claude, until);
        assert_eq!(limits.exhausted_until.len(), 4);
        limits.mark(ModelTier::new("gpt-6-luna"), until + Duration::minutes(5));
        assert_eq!(limits.provider_until(Provider::Claude, now()), Some(until));
        assert_eq!(limits.provider_until(Provider::Codex, now()), Some(until + Duration::minutes(5)));
        assert_eq!(limits.provider_until(Provider::Gemini, now()), None);
        limits.clear_provider(Provider::Claude);
        assert_eq!(limits.exhausted_until.len(), 1);
        assert!(limits.is_exhausted(&ModelTier::new("gpt-6-luna"), now()));
        limits.clear_provider(Provider::Codex);
        assert!(limits.is_empty());
    }

    #[test]
    fn rate_limit_expiry_drives_retry_when_it_is_the_only_blocker() {
        let mut cfg = BudgetConfig::default();
        for m in cfg.providers.claude.models.values_mut() {
            m.enabled = false;
        }
        cfg.providers.claude.models.get_mut(&fable()).unwrap().enabled = true;
        let l = ledger(&cfg, 0.1, &[]);
        let mut limits = RateLimitState::default();
        let until = now() + Duration::minutes(7);
        limits.mark(fable(), until);
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), None);
        assert_eq!(d.model, None);
        assert_eq!(d.retry_at, Some(until));
    }

    #[test]
    fn override_downgrades_but_never_upgrades() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.1, &[]);
        let limits = RateLimitState::default();
        let mut t = task(Criticality::Normal);
        t.model_override = Some(fable());
        let d = decide(&cfg, &l, &limits, &t, None);
        assert_eq!(d.model, Some(sonnet()), "fable and opus are reserved; next eligible below fable is sonnet");
        assert!(d.reasons.iter().any(|r| r.contains("preferred fable not eligible; downgraded to sonnet")), "{:?}", d.reasons);

        let mut t = task(Criticality::Critical);
        t.model_override = Some(sonnet());
        let d = decide(&cfg, &l, &limits, &t, Some(fable()));
        assert_eq!(d.model, Some(sonnet()), "task override beats the rules preference and is never upgraded");
        assert!(d.reasons.iter().any(|r| r == "model override: sonnet"), "{:?}", d.reasons);
    }

    #[test]
    fn override_of_cheapest_tier_when_it_is_out_yields_nothing() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.1, &[(haiku(), 1.0)]);
        let mut t = task(Criticality::Critical);
        t.model_override = Some(haiku());
        let d = decide(&cfg, &l, &RateLimitState::default(), &t, None);
        assert_eq!(d.model, None);
        assert_eq!(d.retry_at, Some(claude(&l).period.end));
        assert!(d.reasons.iter().any(|r| r.contains("no cheaper tier is")), "{:?}", d.reasons);
    }

    #[test]
    fn override_of_another_providers_model_never_crosses_providers() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.1, &[]);
        let mut t = task(Criticality::Critical);
        t.model_override = Some(ModelTier::new("gpt-6.1-sol"));
        let d = decide(&cfg, &l, &RateLimitState::default(), &t, None);
        assert_eq!(d.model, None, "codex is disabled and the chain never reaches claude models");
        assert!(
            d.reasons.iter().any(|r| r.contains("preferred gpt-6.1-sol not eligible and no cheaper tier is")),
            "{:?}",
            d.reasons
        );
    }

    #[test]
    fn cheapest_tier_remains_when_expensive_ones_are_exhausted() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.5, &[(fable(), 1.0), (opus(), 1.0), (sonnet(), 0.999)]);
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::Normal), None);
        assert_eq!(d.model, Some(haiku()));
        assert!(d.reasons.iter().any(|r| r.starts_with("sonnet: period budget:")), "{:?}", d.reasons);
        assert!(
            d.reasons.iter().any(|r| r.contains("default model sonnet not eligible; downgraded to haiku")),
            "{:?}",
            d.reasons
        );
    }

    #[test]
    fn everything_exhausted_waits_for_the_period_end() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.5, &[(fable(), 1.0), (opus(), 1.0), (sonnet(), 1.0), (haiku(), 1.0)]);
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::Critical), None);
        assert_eq!(d.model, None);
        assert_eq!(d.retry_at, Some(claude(&l).period.end));
    }

    #[test]
    fn safety_margin_keeps_the_last_slice_unspent() {
        let cfg = BudgetConfig { safety_margin: 0.10, ..BudgetConfig::default() };
        let l = ledger(&cfg, 0.5, &[(sonnet(), 0.85)]);
        let limits = RateLimitState::default();
        let policy = Policy::new(&cfg, &l, &limits);
        let remaining = claude(&l).tier(&sonnet()).period_remaining();
        let (ok, _) = policy.eligibility(&task(Criticality::Normal), &sonnet(), remaining * 0.95);
        assert!(!ok, "fits the raw remainder but not the margin");
        let (ok, why) = policy.eligibility(&task(Criticality::Normal), &sonnet(), remaining * 0.85);
        assert!(ok, "{why}");
    }

    #[test]
    fn calibration_can_exhaust_the_overall_budget() {
        let cfg = BudgetConfig::default();
        let mut l = ledger(&cfg, 0.5, &[]);
        claude_mut(&mut l).calibration = Some(Calibration { observed_fraction: 0.97, at: now(), measured_fraction: 0.0 });
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::Critical), None);
        assert_eq!(d.model, None);
        assert!(d.reasons.iter().any(|r| r.contains("overall period budget")), "{:?}", d.reasons);
    }

    #[test]
    fn low_tasks_use_the_low_model_and_rules_preference_applies() {
        let cfg = BudgetConfig { low_model: haiku(), ..BudgetConfig::default() };
        let l = ledger(&cfg, 0.1, &[]);
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::Low), None);
        assert_eq!(d.model, Some(haiku()));
        let d = decide(&cfg, &l, &limits, &task(Criticality::Low), Some(sonnet()));
        assert_eq!(d.model, Some(sonnet()));
        assert!(d.reasons.iter().any(|r| r == "preferred by rules: sonnet"), "{:?}", d.reasons);
    }

    #[test]
    fn disabled_tiers_are_never_candidates() {
        let mut cfg = BudgetConfig::default();
        cfg.providers.claude.models.get_mut(&fable()).unwrap().enabled = false;
        cfg.providers.claude.models.get_mut(&opus()).unwrap().share = 0.0;
        let l = ledger(&cfg, 0.1, &[]);
        let limits = RateLimitState::default();
        let policy = Policy::new(&cfg, &l, &limits);
        assert_eq!(policy.candidates(), vec![sonnet(), haiku()]);
        let d = policy.decide(&task(Criticality::Critical), prediction(1.0), None);
        assert_eq!(d.model, Some(sonnet()));
        let (ok, why) = policy.eligibility(&task(Criticality::Critical), &fable(), 1.0);
        assert!(!ok);
        assert_eq!(why, "disabled or no budget share");
    }

    #[test]
    fn disabled_claude_provider_decides_nothing() {
        let cfg = BudgetConfig::default();
        let ledgers = Ledgers::default();
        let limits = RateLimitState::default();
        let policy = Policy::new(&cfg, &ledgers, &limits);
        assert!(policy.candidates().is_empty());
        let d = policy.decide(&task(Criticality::Critical), prediction(1.0), None);
        assert_eq!(d.model, None);
        assert!(d.retry_at.is_some());
        assert!(d.reasons[0].contains("claude provider is disabled"), "{:?}", d.reasons);
        let (ok, why) = policy.eligibility(&task(Criticality::Critical), &fable(), 1.0);
        assert!(!ok);
        assert_eq!(why, "provider claude is disabled");
    }

    #[test]
    fn criticality_block_retries_at_the_next_relaxation_boundary() {
        let mut cfg = BudgetConfig::default();
        for m in cfg.providers.claude.models.values_mut() {
            m.enabled = false;
        }
        cfg.providers.claude.models.get_mut(&fable()).unwrap().enabled = true;
        let l = ledger(&cfg, 0.1, &[]);
        let limits = RateLimitState::default();
        let period = claude(&l).period;
        // High needs one level: wait for relax_after_fraction (0.5).
        let d = decide(&cfg, &l, &limits, &task(Criticality::High), None);
        assert_eq!(d.model, None);
        assert_eq!(d.retry_at, Some(period.start + fraction_of(period.len(), 0.5)));
        // Normal needs two levels: wait for the end game (0.8).
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), None);
        assert_eq!(d.retry_at, Some(period.start + fraction_of(period.len(), 0.8)));
        // Low can never get fable: period end.
        let d = decide(&cfg, &l, &limits, &task(Criticality::Low), None);
        assert_eq!(d.retry_at, Some(period.end));
    }

    #[test]
    fn level_helpers() {
        assert_eq!(lower_by(Criticality::Critical, 1), Criticality::High);
        assert_eq!(lower_by(Criticality::Critical, 2), Criticality::Normal);
        assert_eq!(lower_by(Criticality::Normal, 5), Criticality::Low);
        assert_eq!(lower_by(Criticality::Low, 0), Criticality::Low);
        assert_eq!(fraction_of(Duration::hours(10), 0.5), Duration::hours(5));
        assert_eq!(fraction_of(Duration::hours(10), 2.0), Duration::hours(10));
    }
}
