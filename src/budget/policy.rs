//! The decision: which model may a task use *now*, on which provider?
//!
//! Candidates are every enabled model (positive share) of every enabled
//! provider, providers walked in `budget.provider_order`; each provider has
//! its own [`Ledger`] (period, window, learned rate, observed usage) in
//! [`Ledgers`]. Capability order and downgrades come from
//! `BudgetConfig::downgrade_chain` and never cross providers.
//!
//! Per-model verdict, in order:
//! 1. The provider must be enabled (have a ledger) and the model must be
//!    enabled with a positive share.
//! 2. The provider must not be on an observed cooldown: a probe reported the
//!    allowance `blocked` or a window/period fully used
//!    ([`super::ObservedUsage::cooldown_until`]).
//! 3. A model that reported `rate_limit` is unavailable until its cooldown
//!    ends (account-wide errors mark every model of the provider).
//! 4. Criticality gate with relaxation: a model is *eligible* for a task when
//!    `task.criticality <= min_criticality`, or when it is relaxed: after
//!    `relax_after_fraction` of the period, if the model's spend fraction is
//!    below the period's elapsed fraction (it is under-paced), one
//!    criticality level lower qualifies; in the end game (`endgame_fraction`)
//!    two levels lower qualify.
//! 5. The predicted cost must fit the provider's period allowance (with the
//!    safety margin; the fraction starts from the provider's own reading
//!    when there is one) and, when the provider has one, its rolling
//!    window. A task *borrowing* a model reserved for more critical work
//!    (relaxation) must also fit what is left of that model's share; work
//!    the model is reserved for is never capped by the share.
//!
//! Choice: the preference list is the hard override (`task.model_override`)
//! when set, else the rules' list (`PRIORITY.md`, in order), else the
//! criticality default (`default_model` for normal, `low_model` for low,
//! none for critical/high). Each preferred model is tried through its
//! downgrade chain. When nothing in the list is eligible and the list was not
//! a hard override, the first eligible candidate in provider order wins (for
//! critical/high work without preferences: the most capable eligible model of
//! the first provider that has one). A hard override never crosses to another
//! provider and is never upgraded.
//!
//! If nothing is chosen, the task is throttled until the earliest instant at
//! which something could change, across providers: a window roll-over
//! (approximated as 15 minutes, because we do not track when the oldest
//! window usage expires), a rate-limit or observed cooldown end, the next
//! relaxation boundary, or a period end — capped by the earliest period end.

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

/// How long an observation that says "exhausted" but carries no reset
/// instant keeps its provider off the candidate list.
pub const OBSERVED_EXHAUSTED_TTL: Duration = Duration::hours(1);

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
    /// When every enabled model of `provider` is cooling down (an
    /// account-wide limit), the soonest of those cooldown ends; `None` when
    /// at least one enabled model is usable or the provider has none.
    pub fn provider_blocked_until(&self, cfg: &BudgetConfig, provider: Provider, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let models = cfg.provider(provider).enabled_models();
        if models.is_empty() {
            return None;
        }
        let mut soonest: Option<DateTime<Utc>> = None;
        for m in &models {
            let until = self.until(m, now)?;
            soonest = Some(soonest.map_or(until, |s| s.min(until)));
        }
        soonest
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

    /// Enabled providers that have a ledger, in `provider_order`.
    pub fn providers(&self) -> Vec<Provider> {
        self.cfg.enabled_providers_in_order().into_iter().filter(|p| self.ledgers.get(*p).is_some()).collect()
    }

    /// Models that can ever be chosen: enabled with a positive share on an
    /// enabled provider; providers in `provider_order`, each provider's
    /// models most capable first.
    pub fn candidates(&self) -> Vec<ModelTier> {
        self.providers().into_iter().flat_map(|p| self.cfg.models_for(p)).filter(|t| tier_share(self.cfg, t) > 0.0).collect()
    }

    /// Decide for `task`, given its predicted cost and the rules' preference
    /// list (`preferred`, most wanted first; empty = no preference).
    /// `task.model_override` wins over `preferred` and never crosses to
    /// another provider. Pure: the same inputs always give the same decision.
    pub fn decide(&self, task: &Task, prediction: Prediction, preferred: &[ModelTier]) -> Decision {
        let providers = self.providers();
        let Some(now) = providers.first().and_then(|p| self.ledgers.get(*p)).map(|l| l.now) else {
            let now = self.ledgers.by_provider.values().next().map(|l| l.now).unwrap_or_else(Utc::now);
            let mut reasons: Vec<String> = Provider::ALL
                .iter()
                .filter(|p| self.ledgers.get(**p).is_none() || !self.cfg.provider(**p).enabled)
                .map(|p| format!("{p} provider is disabled (budget.providers.{p}.enabled = false)"))
                .collect();
            reasons.push("no provider is enabled; nothing can run".to_string());
            return Decision { model: None, retry_at: Some(now + WINDOW_RECHECK), prediction, reasons };
        };
        let mut reasons = Vec::new();
        reasons.push(format!("predicted cost {:.0} weighted tokens ({})", prediction.weighted_tokens, prediction.basis));
        for p in &providers {
            if let Some(ledger) = self.ledgers.get(*p) {
                let window = if ledger.has_window() {
                    format!("window {:.0}% used ({})", ledger.window_fraction() * 100.0, ledger.window_fraction_source())
                } else {
                    "no window".to_string()
                };
                reasons.push(format!(
                    "{p}: period {:.0}% elapsed, {:.0}% spent; {window}",
                    ledger.elapsed_fraction() * 100.0,
                    ledger.period_fraction() * 100.0,
                ));
            }
        }

        let candidates = self.candidates();
        let mut eligible = Vec::new();
        let mut retries: Vec<(Provider, DateTime<Utc>)> = Vec::new();
        for tier in &candidates {
            let v = self.verdict(task, tier, prediction.weighted_tokens);
            reasons.push(format!("{tier}: {}", v.reason));
            if v.eligible {
                eligible.push(tier.clone());
            } else if let Some(at) = v.retry_at {
                retries.push((tier.provider(), at));
            }
        }

        // The preference list, whether it is a hard override, and its label.
        let (list, hard, label): (Vec<ModelTier>, bool, &str) = match &task.model_override {
            Some(t) => {
                reasons.push(format!("model override: {t}"));
                (vec![t.clone()], true, "preferred")
            }
            None if !preferred.is_empty() => {
                let names: Vec<String> = preferred.iter().map(|m| m.to_string()).collect();
                reasons.push(format!("preferred by rules: {}", names.join(" | ")));
                (preferred.to_vec(), false, "preferred")
            }
            None => match task.criticality {
                Criticality::Critical | Criticality::High => (Vec::new(), false, ""),
                Criticality::Normal => (vec![self.cfg.default_model.clone()], false, "default model"),
                Criticality::Low => (vec![self.cfg.low_model.clone()], false, "low model"),
            },
        };

        let mut model = None;
        for p in &list {
            if let Some(t) = self.pick_at_or_below(&eligible, p, &mut reasons, &format!("{label} {p}")) {
                model = Some(t);
                break;
            }
        }
        if model.is_none() && !hard {
            model = eligible.first().cloned();
            if let Some(t) = &model {
                if list.is_empty() {
                    reasons.push(format!("{} task: most capable eligible tier is {t}", task.criticality));
                } else {
                    reasons.push(format!(
                        "nothing in the preference list is eligible; falling back to {t} (first eligible on {} in provider order)",
                        t.provider()
                    ));
                }
            }
        }

        let retry_at = if model.is_some() {
            None
        } else {
            // A hard override can only ever run on its own provider.
            let relevant = |p: &Provider| task.model_override.as_ref().is_none_or(|o| o.provider() == *p);
            let mut at = retries.into_iter().filter(|(p, _)| relevant(p)).map(|(_, at)| at).min();
            if let Some(end) = self.ledgers.earliest_period_end() {
                at = Some(at.map_or(end, |a| a.min(end)));
            }
            let mut at = at.unwrap_or(now + WINDOW_RECHECK);
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
        if let Some((reason, retry_at)) = observed_block(ledger) {
            return blocked(reason, retry_at);
        }
        if let Some(until) = self.rate_limits.until(tier, now) {
            let scope = if self.rate_limits.provider_blocked_until(self.cfg, tier.provider(), now).is_some() {
                format!(" ({} account-wide)", tier.provider())
            } else {
                String::new()
            };
            return blocked(
                format!("rate limited until {}{scope}", until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
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
        // A share caps what a model lends to less critical work; work the
        // model is reserved for is limited by the whole allowance only.
        // (`Criticality` orders most important first: "greater" = less critical.)
        let borrowed = task.criticality > model.min_criticality;
        let available = tl.period_remaining() * margin;
        if borrowed && cost > available {
            return blocked(
                format!("{tier}'s share: needs {:.0} weighted tokens, {:.0} left after the safety margin", cost, available),
                Some(period.end),
            );
        }
        let overall_after = ledger.period_fraction() + cost / ledger.period_budget.max(1.0);
        if overall_after > margin {
            return blocked(
                format!(
                    "period allowance: {:.0}% used ({}), this task would push it to {:.0}%",
                    ledger.period_fraction() * 100.0,
                    ledger.period_fraction_source(),
                    overall_after * 100.0
                ),
                Some(period.end),
            );
        }
        if ledger.has_window() {
            let window_after = ledger.window_fraction() + cost / ledger.window_divisor();
            if window_after > margin {
                let reset = ledger.observed.as_ref().and_then(|o| o.window_resets_at).filter(|r| *r > now);
                return blocked(
                    format!(
                        "window: {:.0}% used ({}), this task would push it to {:.0}%",
                        ledger.window_fraction() * 100.0,
                        ledger.window_fraction_source(),
                        window_after * 100.0
                    ),
                    Some(reset.map_or(now + WINDOW_RECHECK, |r| r.min(now + WINDOW_RECHECK))),
                );
            }
        }

        let how = if levels > 0 { format!("eligible (relaxed to {allowed})") } else { "eligible".to_string() };
        Verdict {
            eligible: true,
            reason: format!("{how}; {:.0}% of its budget spent at {:.0}% of the period", s * 100.0, e * 100.0),
            retry_at: None,
        }
    }
}

/// Why a whole provider is unusable right now according to its latest
/// observation (blocked, or a window/period fully used), with when that may
/// change. An exhausted observation without a reset instant counts for
/// [`OBSERVED_EXHAUSTED_TTL`] after it was taken; one whose reset has passed
/// counts no more.
fn observed_block(ledger: &Ledger) -> Option<(String, Option<DateTime<Utc>>)> {
    let obs = ledger.observed.as_ref()?;
    let now = ledger.now;
    let window_out = obs.window_used.is_some_and(|u| u >= 1.0);
    let period_out = obs.period_used.is_some_and(|u| u >= 1.0);
    if !(obs.blocked || window_out || period_out) {
        return None;
    }
    let what = if obs.blocked {
        "reports its allowance blocked"
    } else if window_out {
        "reports its window fully used"
    } else {
        "reports its period allowance fully used"
    };
    match obs.cooldown_until() {
        Some(until) if until > now => Some((
            format!(
                "{} {what}; skipped until the reset at {}",
                ledger.provider,
                until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
            Some(until),
        )),
        Some(_) => None,
        None if now - obs.observed_at < OBSERVED_EXHAUSTED_TTL => Some((
            format!(
                "{} {what} (no reset time known; observed {} min ago)",
                ledger.provider,
                (now - obs.observed_at).num_minutes()
            ),
            Some(now + WINDOW_RECHECK),
        )),
        None => None,
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
    use crate::budget::ledger::TierLedger;
    use crate::budget::period::Period;
    use crate::budget::probe::ObservedUsage;
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
        let mut l = Ledger::blank(Provider::Claude, now, period, Period { start: now - Duration::hours(5), end: now });
        l.total_period_weighted = tiers.iter().map(|t| t.period_weighted).sum();
        l.tiers = tiers;
        l.period_budget = claude.period_weighted_tokens as f64;
        l.window_budget = claude.window_weighted_tokens as f64;
        l.configured_period_budget = l.period_budget;
        l.configured_window_budget = l.window_budget;
        Ledgers::single(l)
    }

    /// A ledger for any provider at `elapsed` of a 7-day period, nothing spent.
    fn provider_ledger(cfg: &BudgetConfig, provider: Provider, elapsed: f64) -> Ledger {
        let now = now();
        let start = now - fraction_of(Duration::hours(PERIOD_HOURS), elapsed);
        let budget = cfg.provider(provider);
        let tiers: Vec<TierLedger> = cfg
            .models_for(provider)
            .into_iter()
            .map(|tier| {
                let b = budget.period_weighted_tokens as f64 * tier_share(cfg, &tier);
                TierLedger { tier, period_budget: b, ..Default::default() }
            })
            .collect();
        let period = Period { start, end: start + Duration::hours(PERIOD_HOURS) };
        let mut l = Ledger::blank(provider, now, period, Period { start: now - Duration::hours(5), end: now });
        l.tiers = tiers;
        l.period_budget = budget.period_weighted_tokens as f64;
        l.window_budget = budget.window_weighted_tokens as f64;
        l.configured_period_budget = l.period_budget;
        l.configured_window_budget = l.window_budget;
        l
    }

    /// Claude and Codex both enabled, both at 10% of their period.
    fn two_providers() -> (BudgetConfig, Ledgers) {
        let mut cfg = BudgetConfig::default();
        cfg.providers.codex.enabled = true;
        let mut ledgers = Ledgers::default();
        for p in [Provider::Claude, Provider::Codex] {
            ledgers.by_provider.insert(p, provider_ledger(&cfg, p, 0.1));
        }
        (cfg, ledgers)
    }

    fn m(name: &str) -> ModelTier {
        ModelTier::new(name)
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

    fn decide(cfg: &BudgetConfig, ledgers: &Ledgers, limits: &RateLimitState, t: &Task, preferred: &[ModelTier]) -> Decision {
        Policy::new(cfg, ledgers, limits).decide(t, prediction(100_000.0), preferred)
    }

    #[test]
    fn fable_is_reserved_for_critical_early_in_the_period() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.1, &[]);
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(fable()));
        assert!(d.retry_at.is_none());
        let d = decide(&cfg, &l, &limits, &task(Criticality::High), &[]);
        assert_eq!(d.model, Some(opus()), "high gets the most capable tier it is allowed: opus");
        assert!(d.reasons.iter().any(|r| r.starts_with("fable: reserved for critical")), "{:?}", d.reasons);
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), &[]);
        assert_eq!(d.model, Some(sonnet()));
    }

    #[test]
    fn fable_relaxes_to_high_after_half_the_period_when_under_spent() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.6, &[(fable(), 0.2)]);
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::High), &[]);
        assert_eq!(d.model, Some(fable()));
        assert!(d.reasons.iter().any(|r| r.contains("fable: eligible (relaxed to high)")), "{:?}", d.reasons);
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), &[]);
        assert_eq!(d.model, Some(sonnet()), "one level of relaxation does not reach normal");
    }

    #[test]
    fn no_relaxation_when_tier_is_over_paced() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.6, &[(fable(), 0.7)]);
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::High), &[]);
        assert_eq!(d.model, Some(opus()));
        assert!(d.reasons.iter().any(|r| r.starts_with("fable: reserved for critical; task is high")), "{:?}", d.reasons);
    }

    #[test]
    fn end_game_opens_fable_to_normal_but_not_low() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.85, &[(fable(), 0.9)]);
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), &[]);
        // Normal prefers the default model (sonnet) even though fable is eligible.
        assert_eq!(d.model, Some(sonnet()));
        assert!(d.reasons.iter().any(|r| r.contains("fable: eligible (relaxed to normal)")), "{:?}", d.reasons);
        let (ok, _) = Policy::new(&cfg, &l, &limits).eligibility(&task(Criticality::Low), &fable(), 1.0);
        assert!(!ok, "two levels below critical is normal, not low");
        // With a preference for fable, normal work now gets it.
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), &[fable()]);
        assert_eq!(d.model, Some(fable()));
    }

    #[test]
    fn window_exhaustion_throttles_with_short_retry() {
        let cfg = BudgetConfig::default();
        let mut l = ledger(&cfg, 0.1, &[]);
        claude_mut(&mut l).total_window_weighted = claude(&l).window_budget - 10.0;
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::Critical), &[]);
        assert_eq!(d.model, None);
        assert_eq!(d.retry_at, Some(now() + WINDOW_RECHECK));
        assert!(d.reasons.iter().any(|r| r.starts_with("fable: window:")), "{:?}", d.reasons);
        assert!(d.reasons.iter().any(|r| r.starts_with("haiku: window:")), "{:?}", d.reasons);
        // Without a window the same spend is no blocker.
        claude_mut(&mut l).window_enabled = false;
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(fable()));
    }

    #[test]
    fn rate_limit_excludes_tier_until_expiry() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.1, &[]);
        let mut limits = RateLimitState::default();
        let until = now() + Duration::minutes(30);
        limits.mark(fable(), until);
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
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
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
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
        let d = decide(&cfg, &l, &limits, &t, &[]);
        assert_eq!(d.model, Some(sonnet()), "fable and opus are reserved; next eligible below fable is sonnet");
        assert!(d.reasons.iter().any(|r| r.contains("preferred fable not eligible; downgraded to sonnet")), "{:?}", d.reasons);

        let mut t = task(Criticality::Critical);
        t.model_override = Some(sonnet());
        let d = decide(&cfg, &l, &limits, &t, &[fable()]);
        assert_eq!(d.model, Some(sonnet()), "task override beats the rules preference and is never upgraded");
        assert!(d.reasons.iter().any(|r| r == "model override: sonnet"), "{:?}", d.reasons);
    }

    #[test]
    fn override_of_cheapest_tier_when_the_allowance_is_out_yields_nothing() {
        let cfg = BudgetConfig::default();
        // Haiku's own share is gone but the allowance has room: a critical
        // task forced onto haiku still runs (shares cap borrowing only).
        let l = ledger(&cfg, 0.1, &[(haiku(), 1.0)]);
        let mut t = task(Criticality::Critical);
        t.model_override = Some(haiku());
        let d = decide(&cfg, &l, &RateLimitState::default(), &t, &[]);
        assert_eq!(d.model, Some(haiku()), "{:?}", d.reasons);
        // The whole allowance gone: nothing, until the period ends.
        let l = ledger(&cfg, 0.1, &[(fable(), 1.0), (opus(), 1.0), (sonnet(), 1.0), (haiku(), 1.0)]);
        let d = decide(&cfg, &l, &RateLimitState::default(), &t, &[]);
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
        let d = decide(&cfg, &l, &RateLimitState::default(), &t, &[]);
        assert_eq!(d.model, None, "codex is disabled and the chain never reaches claude models");
        assert!(
            d.reasons.iter().any(|r| r.contains("preferred gpt-6.1-sol not eligible and no cheaper tier is")),
            "{:?}",
            d.reasons
        );
    }

    #[test]
    fn a_spent_share_caps_borrowed_use_but_not_reserved_use() {
        let cfg = BudgetConfig::default();
        // Fable's whole share is gone at 60% of the period; the allowance as a whole is 25% used.
        let l = ledger(&cfg, 0.6, &[(fable(), 1.0)]);
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(fable()), "critical work is what fable is reserved for: the share does not cap it");
        assert!(d.reasons.iter().any(|r| r.starts_with("fable: eligible")), "{:?}", d.reasons);
        // A high task may only borrow fable while its share has room.
        let mut t = task(Criticality::High);
        t.model_override = Some(fable());
        let d = decide(&cfg, &l, &limits, &t, &[]);
        assert_eq!(d.model, Some(opus()), "{:?}", d.reasons);
        assert!(d.reasons.iter().any(|r| r.starts_with("fable: reserved for critical; task is high")), "{:?}", d.reasons);
        // Under-paced opus lends itself to normal work only within what is left of its share.
        let mut l = ledger(&cfg, 0.6, &[(opus(), 0.55)]);
        claude_mut(&mut l).window_enabled = false;
        let policy = Policy::new(&cfg, &l, &limits);
        let left = claude(&l).tier(&opus()).period_remaining() * 0.95;
        let w = tier_weight(&cfg, &opus());
        let (ok, why) = policy.eligibility(&task(Criticality::Normal), &opus(), left / w * 1.1);
        assert!(!ok && why.starts_with("opus's share: needs"), "{why}");
        let (ok, why) = policy.eligibility(&task(Criticality::Normal), &opus(), left / w * 0.9);
        assert!(ok, "{why}");
        let (ok, why) = policy.eligibility(&task(Criticality::High), &opus(), left / w * 1.1);
        assert!(ok, "high work is what opus is reserved for: {why}");
    }

    #[test]
    fn the_whole_allowance_caps_everything() {
        let cfg = BudgetConfig::default();
        // 95% of the allowance used: nothing fits inside the 5% safety margin.
        let l = ledger(&cfg, 0.5, &[(fable(), 1.0), (opus(), 1.0), (sonnet(), 1.0)]);
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::Normal), &[]);
        assert_eq!(d.model, None, "{:?}", d.reasons);
        assert!(d.reasons.iter().any(|r| r.starts_with("sonnet: period allowance: 95% used (measured)")), "{:?}", d.reasons);
        assert!(d.reasons.iter().any(|r| r.starts_with("haiku: period allowance: 95% used (measured)")), "{:?}", d.reasons);
        // The retry waits for the earliest thing that could change: fable's
        // and opus's criticality gates relax in the end game, before the period ends.
        assert!(d.retry_at.is_some_and(|at| at > now() && at <= claude(&l).period.end), "{:?}", d.retry_at);
    }

    #[test]
    fn everything_exhausted_waits_for_the_period_end() {
        let cfg = BudgetConfig::default();
        let l = ledger(&cfg, 0.5, &[(fable(), 1.0), (opus(), 1.0), (sonnet(), 1.0), (haiku(), 1.0)]);
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::Critical), &[]);
        assert_eq!(d.model, None);
        assert_eq!(d.retry_at, Some(claude(&l).period.end));
    }

    #[test]
    fn safety_margin_keeps_the_last_slice_unspent() {
        let cfg = BudgetConfig { safety_margin: 0.10, ..BudgetConfig::default() };
        // 85% of the allowance used (sonnet's share is 35%: 0.35 × 2.43 ≈ 0.85).
        let l = ledger(&cfg, 0.5, &[(sonnet(), 0.85 / 0.35)]);
        let limits = RateLimitState::default();
        let policy = Policy::new(&cfg, &l, &limits);
        let remaining = claude(&l).period_budget * (1.0 - claude(&l).period_fraction());
        let (ok, _) = policy.eligibility(&task(Criticality::Normal), &sonnet(), remaining * 0.95);
        assert!(!ok, "fits the raw remainder but not the margin");
        let (ok, why) = policy.eligibility(&task(Criticality::Normal), &sonnet(), remaining * 0.30);
        assert!(ok, "{why}");
    }

    #[test]
    fn the_providers_own_reading_can_exhaust_the_allowance() {
        let cfg = BudgetConfig::default();
        let mut l = ledger(&cfg, 0.5, &[]);
        claude_mut(&mut l).observed = Some(ObservedUsage { period_used: Some(0.97), ..ObservedUsage::empty(now()) });
        let d = decide(&cfg, &l, &RateLimitState::default(), &task(Criticality::Critical), &[]);
        assert_eq!(d.model, None);
        assert!(d.reasons.iter().any(|r| r.contains("period allowance: 97% used (observed)")), "{:?}", d.reasons);
    }

    #[test]
    fn an_observed_window_counts_even_without_a_configured_one() {
        let cfg = BudgetConfig::default();
        let mut l = ledger(&cfg, 0.5, &[]);
        let reset = now() + Duration::minutes(40);
        let limits = RateLimitState::default();
        claude_mut(&mut l).window_enabled = false;
        claude_mut(&mut l).observed =
            Some(ObservedUsage { window_used: Some(0.5), window_resets_at: Some(reset), ..ObservedUsage::empty(now()) });
        let policy = Policy::new(&cfg, &l, &limits);
        // 1.2M × fable's weight 5 = 6M: fine for the 80M period, not for half of a 12M window.
        let (ok, why) = policy.eligibility(&task(Criticality::Critical), &fable(), 1_200_000.0);
        assert!(!ok, "config says no window but the provider has one: {why}");
        assert!(why.starts_with("window: 50% used (observed)"), "{why}");
        // The window is 12M weighted tokens; 40% used leaves room for 1M × 5.
        claude_mut(&mut l).observed.as_mut().unwrap().window_used = Some(0.4);
        let policy = Policy::new(&cfg, &l, &limits);
        let (ok, why) = policy.eligibility(&task(Criticality::Critical), &fable(), 1_000_000.0);
        assert!(ok, "{why}");
        // When the window is the blocker, the retry waits for its reset (when sooner than the recheck).
        // 40M: too big for the period on the expensive tiers, too big for the window even on haiku.
        let d = policy.decide(&task(Criticality::Critical), prediction(40_000_000.0), &[]);
        assert_eq!(d.model, None, "{:?}", d.reasons);
        assert_eq!(d.retry_at, Some(now() + WINDOW_RECHECK), "reset in 40 min is later than the 15 min recheck");
        claude_mut(&mut l).observed.as_mut().unwrap().window_resets_at = Some(now() + Duration::minutes(4));
        let policy = Policy::new(&cfg, &l, &limits);
        let d = policy.decide(&task(Criticality::Critical), prediction(40_000_000.0), &[]);
        assert_eq!(d.retry_at, Some(now() + Duration::minutes(4)));
    }

    #[test]
    fn low_tasks_use_the_low_model_and_rules_preference_applies() {
        let cfg = BudgetConfig { low_model: haiku(), ..BudgetConfig::default() };
        let l = ledger(&cfg, 0.1, &[]);
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::Low), &[]);
        assert_eq!(d.model, Some(haiku()));
        let d = decide(&cfg, &l, &limits, &task(Criticality::Low), &[sonnet()]);
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
        let d = policy.decide(&task(Criticality::Critical), prediction(1.0), &[]);
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
        let d = policy.decide(&task(Criticality::Critical), prediction(1.0), &[]);
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
        let d = decide(&cfg, &l, &limits, &task(Criticality::High), &[]);
        assert_eq!(d.model, None);
        assert_eq!(d.retry_at, Some(period.start + fraction_of(period.len(), 0.5)));
        // Normal needs two levels: wait for the end game (0.8).
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), &[]);
        assert_eq!(d.retry_at, Some(period.start + fraction_of(period.len(), 0.8)));
        // Low can never get fable: period end.
        let d = decide(&cfg, &l, &limits, &task(Criticality::Low), &[]);
        assert_eq!(d.retry_at, Some(period.end));
    }

    #[test]
    fn second_provider_used_when_first_is_rate_limited() {
        let (cfg, l) = two_providers();
        let mut limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(fable()), "claude comes first in the default order");
        limits.mark_provider(&cfg, Provider::Claude, now() + Duration::minutes(30));
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(m("gpt-6.1-sol")));
        assert!(
            d.reasons.iter().any(|r| r == "fable: rate limited until 2026-10-01T12:30:00Z (claude account-wide)"),
            "{:?}",
            d.reasons
        );
        assert!(d.reasons.iter().any(|r| r.starts_with("gpt-6.1-sol: eligible")), "{:?}", d.reasons);
        // Normal work: the default model is out, so the first eligible codex model wins.
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), &[]);
        assert_eq!(d.model, Some(m("gpt-6-luna")), "{:?}", d.reasons);
        assert!(d.reasons.iter().any(|r| r.contains("falling back to gpt-6-luna")), "{:?}", d.reasons);
    }

    #[test]
    fn preference_list_is_tried_in_order() {
        let (cfg, l) = two_providers();
        let mut limits = RateLimitState::default();
        let prefs = [m("gpt-6-astra"), opus()];
        let d = decide(&cfg, &l, &limits, &task(Criticality::High), &prefs);
        assert_eq!(d.model, Some(m("gpt-6-astra")));
        assert!(d.reasons.iter().any(|r| r == "preferred by rules: gpt-6-astra | opus"), "{:?}", d.reasons);
        let d = decide(&cfg, &l, &limits, &task(Criticality::High), &[opus(), m("gpt-6-astra")]);
        assert_eq!(d.model, Some(opus()));
        // The first preference's whole chain is out: the second preference wins.
        limits.mark_provider(&cfg, Provider::Codex, now() + Duration::minutes(30));
        let d = decide(&cfg, &l, &limits, &task(Criticality::High), &prefs);
        assert_eq!(d.model, Some(opus()));
        assert!(d.reasons.iter().any(|r| r == "preferred gpt-6-astra not eligible and no cheaper tier is"), "{:?}", d.reasons);
        assert!(d.reasons.iter().any(|r| r == "preferred opus is eligible"), "{:?}", d.reasons);
        // Only astra is out: its own chain (gpt-6-luna) comes before the next preference.
        let mut limits = RateLimitState::default();
        limits.mark(m("gpt-6-astra"), now() + Duration::minutes(30));
        let d = decide(&cfg, &l, &limits, &task(Criticality::Normal), &prefs);
        assert_eq!(d.model, Some(m("gpt-6-luna")));
    }

    #[test]
    fn override_never_crosses_provider() {
        let (cfg, l) = two_providers();
        let mut limits = RateLimitState::default();
        let until = now() + Duration::minutes(20);
        limits.mark_provider(&cfg, Provider::Codex, until);
        limits.mark(fable(), now() + Duration::minutes(5));
        let mut t = task(Criticality::Critical);
        t.model_override = Some(m("gpt-6-astra"));
        let d = decide(&cfg, &l, &limits, &t, &[fable()]);
        assert_eq!(d.model, None, "claude is free but the override pins codex: {:?}", d.reasons);
        assert_eq!(d.retry_at, Some(until), "only the override's provider decides when to look again");
        assert!(d.reasons.iter().any(|r| r == "preferred gpt-6-astra not eligible and no cheaper tier is"), "{:?}", d.reasons);
    }

    #[test]
    fn provider_order_drives_fallback() {
        let (mut cfg, l) = two_providers();
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(fable()));
        cfg.provider_order = vec![Provider::Codex, Provider::Claude];
        let policy = Policy::new(&cfg, &l, &limits);
        assert_eq!(policy.providers(), vec![Provider::Codex, Provider::Claude]);
        assert_eq!(policy.candidates()[0], m("gpt-6.1-sol"));
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(m("gpt-6.1-sol")));
        // A preference still beats the order.
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[fable()]);
        assert_eq!(d.model, Some(fable()));
        // Order only lists codex: claude still follows as a fallback.
        cfg.provider_order = vec![Provider::Codex];
        assert_eq!(Policy::new(&cfg, &l, &limits).providers(), vec![Provider::Codex, Provider::Claude]);
    }

    #[test]
    fn disabled_provider_is_never_a_candidate() {
        let (mut cfg, l) = two_providers();
        cfg.providers.codex.enabled = false;
        let limits = RateLimitState::default();
        let policy = Policy::new(&cfg, &l, &limits);
        assert_eq!(policy.providers(), vec![Provider::Claude], "a stale codex ledger does not make codex a candidate");
        assert!(policy.candidates().iter().all(|t| t.provider() == Provider::Claude));
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[m("gpt-6.1-sol")]);
        assert_eq!(d.model, Some(fable()), "a rules preference for a disabled provider falls back: {:?}", d.reasons);
        assert!(d.reasons.iter().any(|r| r.contains("falling back to fable")), "{:?}", d.reasons);
        assert!(!d.reasons.iter().any(|r| r.starts_with("gpt-6.1-sol:")), "{:?}", d.reasons);
    }

    #[test]
    fn observed_blocked_provider_is_skipped_until_reset() {
        let (cfg, mut l) = two_providers();
        let reset = now() + Duration::hours(2);
        let obs = ObservedUsage {
            window_used: Some(0.4),
            window_resets_at: Some(reset),
            blocked: true,
            ..ObservedUsage::empty(now() - Duration::minutes(3))
        };
        claude_mut(&mut l).observed = Some(obs.clone());
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(m("gpt-6.1-sol")));
        assert!(
            d.reasons
                .iter()
                .any(|r| r == "fable: claude reports its allowance blocked; skipped until the reset at 2026-10-01T14:00:00Z"),
            "{:?}",
            d.reasons
        );

        // Claude alone: throttled until the observed reset.
        let mut only = Ledgers::single(claude(&l).clone());
        let cfg_claude = BudgetConfig::default();
        let d = decide(&cfg_claude, &only, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, None);
        assert_eq!(d.retry_at, Some(reset));

        // A full window without `blocked` behaves the same.
        claude_mut(&mut only).observed = Some(ObservedUsage { blocked: false, window_used: Some(1.0), ..obs.clone() });
        let d = decide(&cfg_claude, &only, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.retry_at, Some(reset));

        // Once the reset has passed the observation no longer blocks.
        claude_mut(&mut only).observed =
            Some(ObservedUsage { window_resets_at: Some(now() - Duration::minutes(1)), ..obs.clone() });
        let d = decide(&cfg_claude, &only, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(fable()));

        // Exhausted without a reset instant: blocked for a while after the observation.
        claude_mut(&mut only).observed = Some(ObservedUsage { window_resets_at: None, ..obs.clone() });
        let d = decide(&cfg_claude, &only, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, None);
        assert_eq!(d.retry_at, Some(now() + WINDOW_RECHECK));
        claude_mut(&mut only).observed =
            Some(ObservedUsage { window_resets_at: None, observed_at: now() - Duration::hours(2), ..obs });
        let d = decide(&cfg_claude, &only, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(fable()), "an old exhausted observation without a reset expires");
    }

    #[test]
    fn an_observed_window_without_any_budget_is_judged_on_the_reading_alone() {
        let cfg = BudgetConfig::default();
        let mut l = ledger(&cfg, 0.5, &[]);
        let limits = RateLimitState::default();
        claude_mut(&mut l).window_enabled = false;
        claude_mut(&mut l).window_budget = 0.0;
        claude_mut(&mut l).observed = Some(ObservedUsage { window_used: Some(0.10), ..ObservedUsage::empty(now()) });
        let policy = Policy::new(&cfg, &l, &limits);
        let (ok, why) = policy.eligibility(&task(Criticality::Critical), &fable(), 2_000_000.0);
        assert!(ok, "{why}");
        claude_mut(&mut l).observed.as_mut().unwrap().window_used = Some(0.96);
        let policy = Policy::new(&cfg, &l, &limits);
        let (ok, why) = policy.eligibility(&task(Criticality::Critical), &fable(), 1.0);
        assert!(!ok && why.starts_with("window: 96% used (observed)"), "{why}");
    }

    #[test]
    fn retry_at_uses_earliest_reset_across_providers() {
        let (cfg, mut l) = two_providers();
        let mut limits = RateLimitState::default();
        limits.mark_provider(&cfg, Provider::Claude, now() + Duration::minutes(50));
        limits.mark_provider(&cfg, Provider::Codex, now() + Duration::minutes(20));
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, None);
        assert_eq!(d.retry_at, Some(now() + Duration::minutes(20)));
        // An observed reset that comes sooner wins.
        let mut limits = RateLimitState::default();
        limits.mark_provider(&cfg, Provider::Claude, now() + Duration::minutes(50));
        let reset = now() + Duration::minutes(7);
        l.get_mut(Provider::Codex).unwrap().observed =
            Some(ObservedUsage { period_used: Some(1.0), period_resets_at: Some(reset), ..ObservedUsage::empty(now()) });
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.retry_at, Some(reset));
        // Capped by the earliest period end across providers.
        let soon = now() + Duration::minutes(3);
        l.get_mut(Provider::Codex).unwrap().observed = None;
        limits.mark_provider(&cfg, Provider::Codex, now() + Duration::hours(3));
        let codex = l.get_mut(Provider::Codex).unwrap();
        codex.period = Period { start: soon - Duration::hours(PERIOD_HOURS), end: soon };
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.retry_at, Some(soon));
    }

    #[test]
    fn provider_without_window_ignores_window_budget() {
        let (mut cfg, mut l) = two_providers();
        cfg.provider_order = vec![Provider::Codex, Provider::Claude];
        let codex = l.get_mut(Provider::Codex).unwrap();
        codex.total_window_weighted = codex.window_budget * 10.0;
        let limits = RateLimitState::default();
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(fable()), "codex's window is full, so claude runs it");
        assert!(d.reasons.iter().any(|r| r.starts_with("gpt-6.1-sol: window:")), "{:?}", d.reasons);
        l.get_mut(Provider::Codex).unwrap().window_enabled = false;
        let d = decide(&cfg, &l, &limits, &task(Criticality::Critical), &[]);
        assert_eq!(d.model, Some(m("gpt-6.1-sol")), "{:?}", d.reasons);
        assert!(d.reasons.iter().any(|r| r == "codex: period 10% elapsed, 0% spent; no window"), "{:?}", d.reasons);
    }

    #[test]
    fn provider_blocked_until_needs_every_enabled_model() {
        let cfg = BudgetConfig::default();
        let mut limits = RateLimitState::default();
        assert_eq!(limits.provider_blocked_until(&cfg, Provider::Claude, now()), None);
        limits.mark(fable(), now() + Duration::minutes(5));
        assert_eq!(limits.provider_blocked_until(&cfg, Provider::Claude, now()), None);
        limits.mark_provider(&cfg, Provider::Claude, now() + Duration::minutes(10));
        limits.mark(opus(), now() + Duration::minutes(3));
        assert_eq!(limits.provider_blocked_until(&cfg, Provider::Claude, now()), Some(now() + Duration::minutes(3)));
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
