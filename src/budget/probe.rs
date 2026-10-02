//! Observed usage: what a provider reports about its own allowance.
//!
//! Where a CLI can tell us the remaining allowance (Codex's
//! `account/rateLimits/read`, Claude Code's status-line `rate_limits`,
//! Antigravity's `/usage`), a [`UsageProbe`] turns it into an
//! [`ObservedUsage`] stored in kv `budget.observed.<provider>`. The ledger
//! uses it two ways ([`apply_observed`]): `period_used` becomes the
//! calibration (same mechanism as `budget set-observed`), and
//! `period_resets_at` overrides the configured anchor for that period. A
//! `blocked` provider, or one whose window is fully used, is on cooldown
//! until the matching reset ([`ObservedUsage::cooldown_until`]).
//!
//! This module holds the types, the trait and the kv helpers. The real
//! probes live with their provider CLIs; [`NoProbe`] is the stand-in.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::Provider;
use crate::store::Store;

use super::ledger::{Calibration, Ledger};

/// A provider's own view of its allowance at `observed_at`. Fractions are
/// `0.0..=1.0` of the allowance used; `None` = the probe could not tell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedUsage {
    /// Fraction of the rolling window used.
    pub window_used: Option<f64>,
    pub window_resets_at: Option<DateTime<Utc>>,
    /// Fraction of the period (weekly) allowance used.
    pub period_used: Option<f64>,
    pub period_resets_at: Option<DateTime<Utc>>,
    /// The provider refuses work right now (hard limit reached).
    pub blocked: bool,
    pub observed_at: DateTime<Utc>,
}

impl ObservedUsage {
    /// An observation that knows nothing (useful as a base for builders).
    pub fn empty(observed_at: DateTime<Utc>) -> Self {
        Self { window_used: None, window_resets_at: None, period_used: None, period_resets_at: None, blocked: false, observed_at }
    }

    /// Until when the provider should not be scheduled, if the observation
    /// says it is exhausted: `blocked` or a fully used window/period, until
    /// the matching reset (the earliest one when both apply). `None` when
    /// nothing is exhausted or no reset instant is known.
    pub fn cooldown_until(&self) -> Option<DateTime<Utc>> {
        let window_out = self.window_used.is_some_and(|u| u >= 1.0);
        let period_out = self.period_used.is_some_and(|u| u >= 1.0);
        let mut until: Option<DateTime<Utc>> = None;
        if self.blocked || window_out {
            until = self.window_resets_at;
        }
        if self.blocked || period_out {
            until = match (until, self.period_resets_at) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        until
    }
}

/// Something that can ask a provider how much allowance is left.
pub trait UsageProbe: Send + Sync {
    fn provider(&self) -> Provider;
    /// Run the probe. `Ok(None)` means "nothing learned" (unsupported plan,
    /// unparseable output); errors are for broken set-ups the daemon should
    /// report. Never blocks for more than a few seconds.
    fn probe(&self) -> Result<Option<ObservedUsage>>;
}

/// A probe that never learns anything; used until a provider's real probe exists.
#[derive(Debug, Clone, Copy)]
pub struct NoProbe(pub Provider);

impl UsageProbe for NoProbe {
    fn provider(&self) -> Provider {
        self.0
    }
    fn probe(&self) -> Result<Option<ObservedUsage>> {
        Ok(None)
    }
}

/// kv key under which a provider's latest [`ObservedUsage`] is stored.
pub fn observed_key(provider: Provider) -> String {
    format!("budget.observed.{provider}")
}

/// The latest observation for a provider, if any. Fails only when the
/// database cannot be read or the stored JSON is unreadable.
pub fn load_observed(store: &Store, provider: Provider) -> Result<Option<ObservedUsage>> {
    store.kv_get(&observed_key(provider)).with_context(|| format!("read observed usage of {provider}"))
}

/// Store a provider's observation (latest wins).
pub fn save_observed(store: &Store, provider: Provider, observed: &ObservedUsage) -> Result<()> {
    store.kv_set(&observed_key(provider), observed).with_context(|| format!("store observed usage of {provider}"))
}

/// Fold an observation into a ledger that was loaded for the same provider.
///
/// * `period_used` becomes the ledger's calibration, like `budget
///   set-observed`: the offset between what the provider reports and what we
///   measured is applied to the period fraction. The observation must fall
///   inside the ledger's period and be newer than an existing manual
///   calibration, otherwise it is ignored.
/// * `period_resets_at` moves the period so it ends at that instant when the
///   ledger's period does not already (the ledger keeps its usage sums; the
///   daemon reloads ledgers every tick so the sums catch up).
/// * The observation itself is attached as `ledger.observed`.
pub fn apply_observed(ledger: &mut Ledger, observed: &ObservedUsage) {
    if let Some(reset) = observed.period_resets_at
        && reset > ledger.now
        && ledger.period.end != reset
    {
        let len = ledger.period.len();
        ledger.period = super::period::Period { start: reset - len, end: reset };
        ledger.anchor_source = super::period::AnchorSource::Observed;
    }
    if let Some(used) = observed.period_used
        && ledger.period.contains(observed.observed_at)
        && ledger.calibration.is_none_or(|c| c.at <= observed.observed_at)
    {
        ledger.calibration = Some(Calibration {
            observed_fraction: used.clamp(0.0, 2.0),
            at: observed.observed_at,
            measured_fraction: ledger.measured_period_fraction(),
        });
    }
    ledger.observed = Some(observed.clone());
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::budget::period::{AnchorSource, Period};

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn ledger(now: DateTime<Utc>) -> Ledger {
        let start = now - Duration::days(2);
        let period = Period { start, end: start + Duration::days(7) };
        Ledger {
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
            calibration: None,
            observed: None,
            anchor_source: AnchorSource::Config,
        }
    }

    #[test]
    fn kv_round_trip() {
        let store = Store::open_in_memory().unwrap();
        assert!(load_observed(&store, Provider::Codex).unwrap().is_none());
        let obs = ObservedUsage { period_used: Some(0.3), ..ObservedUsage::empty(at("2026-10-01T12:00:00Z")) };
        save_observed(&store, Provider::Codex, &obs).unwrap();
        assert_eq!(load_observed(&store, Provider::Codex).unwrap(), Some(obs));
        assert!(load_observed(&store, Provider::Claude).unwrap().is_none());
        assert_eq!(observed_key(Provider::Gemini), "budget.observed.gemini");
        assert!(NoProbe(Provider::Gemini).probe().unwrap().is_none());
        assert_eq!(NoProbe(Provider::Gemini).provider(), Provider::Gemini);
    }

    #[test]
    fn cooldown_follows_the_exhausted_bucket() {
        let now = at("2026-10-01T12:00:00Z");
        let mut obs = ObservedUsage::empty(now);
        assert_eq!(obs.cooldown_until(), None);
        obs.window_used = Some(0.5);
        obs.window_resets_at = Some(now + Duration::hours(1));
        obs.period_resets_at = Some(now + Duration::days(3));
        assert_eq!(obs.cooldown_until(), None);
        obs.window_used = Some(1.0);
        assert_eq!(obs.cooldown_until(), Some(now + Duration::hours(1)));
        obs.window_used = Some(0.2);
        obs.period_used = Some(1.0);
        assert_eq!(obs.cooldown_until(), Some(now + Duration::days(3)));
        obs.blocked = true;
        assert_eq!(obs.cooldown_until(), Some(now + Duration::hours(1)), "blocked waits for the earliest reset");
        obs.window_resets_at = None;
        assert_eq!(obs.cooldown_until(), Some(now + Duration::days(3)));
    }

    #[test]
    fn apply_sets_calibration_and_moves_the_period() {
        let now = at("2026-10-01T12:00:00Z");
        let mut l = ledger(now);
        let obs = ObservedUsage {
            period_used: Some(0.4),
            period_resets_at: Some(now + Duration::days(1)),
            ..ObservedUsage::empty(now - Duration::minutes(5))
        };
        apply_observed(&mut l, &obs);
        assert_eq!(l.period.end, now + Duration::days(1));
        assert_eq!(l.period.len(), Duration::days(7));
        assert_eq!(l.anchor_source, AnchorSource::Observed);
        let cal = l.calibration.unwrap();
        assert_eq!(cal.observed_fraction, 0.4);
        assert_eq!(cal.measured_fraction, 0.1);
        assert!((l.period_fraction() - 0.4).abs() < 1e-9);
        assert_eq!(l.observed, Some(obs));
    }

    #[test]
    fn apply_respects_newer_manual_calibration_and_stale_observations() {
        let now = at("2026-10-01T12:00:00Z");
        let mut l = ledger(now);
        let manual = Calibration { observed_fraction: 0.7, at: now, measured_fraction: 0.1 };
        l.calibration = Some(manual);
        let obs = ObservedUsage { period_used: Some(0.4), ..ObservedUsage::empty(now - Duration::hours(1)) };
        apply_observed(&mut l, &obs);
        assert_eq!(l.calibration, Some(manual), "a newer manual calibration wins");
        assert_eq!(l.anchor_source, AnchorSource::Config);

        let mut l = ledger(now);
        let stale = ObservedUsage { period_used: Some(0.4), ..ObservedUsage::empty(now - Duration::days(10)) };
        apply_observed(&mut l, &stale);
        assert!(l.calibration.is_none(), "an observation from another period is ignored");
        assert!(l.observed.is_some());

        let mut l = ledger(now);
        let past_reset = ObservedUsage { period_resets_at: Some(now - Duration::hours(1)), ..ObservedUsage::empty(now) };
        let before = l.period;
        apply_observed(&mut l, &past_reset);
        assert_eq!(l.period, before, "a reset in the past does not move the period");
    }
}
