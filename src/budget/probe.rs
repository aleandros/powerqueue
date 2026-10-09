//! Observed usage: what a provider reports about its own allowance.
//!
//! Where a CLI can tell us the remaining allowance (Codex's
//! `account/rateLimits/read`, Claude Code's status-line `rate_limits`,
//! Antigravity's `/usage`), a [`UsageProbe`] turns it into an
//! [`ObservedUsage`] stored in kv `budget.observed.<provider>` (latest
//! wins) plus one [`ObservationSample`] appended to kv
//! `budget.observations.<provider>` (the history the ledger learns the
//! exchange rate from; see `ledger::LearnedRate`). The ledger uses the
//! latest observation as the truth for the period and window fractions and
//! only adds what it measured since ([`apply_observed`]);
//! `period_resets_at` overrides the configured anchor. A `blocked`
//! provider, or one whose window is fully used, is on cooldown until the
//! matching reset ([`ObservedUsage::cooldown_until`]).
//!
//! This module holds the types, the trait and the pure history rules; the
//! kv reads and writes live in [`super::io`]. The real probes live with
//! their provider CLIs; [`NoProbe`] is the stand-in.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::Provider;

use super::ledger::Ledger;

/// One reading kept for learning the exchange rate between our weighted
/// tokens and the provider's own percentages.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ObservationSample {
    pub at: DateTime<Utc>,
    pub period_used: Option<f64>,
    pub window_used: Option<f64>,
}

impl ObservationSample {
    pub fn of(obs: &ObservedUsage) -> Self {
        Self { at: obs.observed_at, period_used: obs.period_used, window_used: obs.window_used }
    }

    /// Neither a period nor a window reading: nothing to record.
    pub fn is_empty(&self) -> bool {
        self.period_used.is_none() && self.window_used.is_none()
    }
}

/// Samples younger than this are kept at most one per [`SAMPLE_SPACING_RECENT`];
/// older ones one per [`SAMPLE_SPACING_OLD`].
pub const SAMPLE_RECENT: Duration = Duration::hours(2);
pub const SAMPLE_SPACING_RECENT: Duration = Duration::seconds(60);
pub const SAMPLE_SPACING_OLD: Duration = Duration::minutes(10);
/// Samples older than this are dropped (longer than any period we pace).
pub const SAMPLE_MAX_AGE: Duration = Duration::days(9);

/// kv key under which a provider's observation history is stored.
pub fn observations_key(provider: Provider) -> String {
    format!("budget.observations.{provider}")
}

/// Fold a pre-0.7 calibration reading (`legacy`, see
/// [`super::io::load_observations`]) into a history when no sample already
/// sits at that instant. Output oldest first.
pub fn fold_legacy_calibration(mut samples: Vec<ObservationSample>, legacy: Option<ObservationSample>) -> Vec<ObservationSample> {
    if let Some(legacy) = legacy
        && !samples.iter().any(|s| (s.at - legacy.at).num_seconds().abs() < 1)
    {
        samples.push(legacy);
        samples.sort_by_key(|s| s.at);
    }
    samples
}

/// The history after appending `obs` and thinning it ([`thin_samples`]), or
/// `None` when there is nothing to store: a reading that knows nothing, or
/// one that only repeats the newest reading within [`SAMPLE_SPACING_RECENT`]
/// (the status line fires after every response). Input oldest first.
pub fn append_observation(mut samples: Vec<ObservationSample>, obs: &ObservedUsage) -> Option<Vec<ObservationSample>> {
    let sample = ObservationSample::of(obs);
    if sample.is_empty() {
        return None;
    }
    if let Some(last) = samples.last()
        && sample.at - last.at < SAMPLE_SPACING_RECENT
        && last.period_used == sample.period_used
        && last.window_used == sample.window_used
    {
        return None;
    }
    samples.push(sample);
    samples.sort_by_key(|s| s.at);
    Some(thin_samples(&samples, sample.at))
}

/// Keep the newest sample, at most one per minute for the last two hours,
/// one per ten minutes before that, nothing older than [`SAMPLE_MAX_AGE`].
/// Input oldest first; output oldest first.
pub fn thin_samples(samples: &[ObservationSample], now: DateTime<Utc>) -> Vec<ObservationSample> {
    let mut kept: Vec<ObservationSample> = Vec::new();
    for s in samples.iter().rev() {
        if now - s.at > SAMPLE_MAX_AGE {
            break;
        }
        let spacing = if now - s.at <= SAMPLE_RECENT { SAMPLE_SPACING_RECENT } else { SAMPLE_SPACING_OLD };
        match kept.last() {
            Some(newer) if newer.at - s.at < spacing => continue,
            _ => kept.push(*s),
        }
    }
    kept.reverse();
    kept
}

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

/// Fold an observation into a ledger that was loaded for the same provider.
///
/// * `period_resets_at` moves the period so it ends at that instant when the
///   ledger's period does not already (the ledger keeps its usage sums; the
///   daemon reloads ledgers every tick so the sums catch up).
/// * The observation is attached as `ledger.observed`; from then on
///   `Ledger::period_fraction` / `window_fraction` start from what the
///   provider reported and add only what was measured since
///   (`spent_since_observation`, which the caller fills in).
pub fn apply_observed(ledger: &mut Ledger, observed: &ObservedUsage) {
    if let Some(reset) = observed.period_resets_at
        && reset > ledger.now
        && ledger.period.end != reset
    {
        let len = ledger.period.len();
        ledger.period = super::period::Period { start: reset - len, end: reset };
        ledger.anchor_source = super::period::AnchorSource::Observed;
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
        let mut l = Ledger::blank(Provider::Claude, now, period, Period { start: now - Duration::hours(5), end: now });
        l.total_period_weighted = 10.0;
        l.period_budget = 100.0;
        l.window_budget = 100.0;
        l.window_enabled = true;
        l
    }

    fn sample(at: DateTime<Utc>, period_used: f64) -> ObservationSample {
        ObservationSample { at, period_used: Some(period_used), window_used: None }
    }

    #[test]
    fn appending_skips_repeats_and_empty_readings_and_stays_sorted() {
        let t0 = at("2026-10-01T12:00:00Z");
        let base = ObservedUsage { period_used: Some(0.3), window_used: Some(0.1), ..ObservedUsage::empty(t0) };
        let h = append_observation(Vec::new(), &base).expect("first reading stored");
        assert_eq!(h, vec![ObservationSample::of(&base)]);
        // Same reading 20s later: not stored. Same reading 2 min later: stored.
        assert!(
            append_observation(h.clone(), &ObservedUsage { observed_at: t0 + Duration::seconds(20), ..base.clone() }).is_none()
        );
        let h = append_observation(h, &ObservedUsage { observed_at: t0 + Duration::minutes(2), ..base.clone() }).unwrap();
        assert_eq!(h.len(), 2);
        // A changed reading 10s later replaces the one just before it (one per minute is kept).
        let changed = ObservedUsage {
            period_used: Some(0.31),
            observed_at: t0 + Duration::minutes(2) + Duration::seconds(10),
            ..base.clone()
        };
        let h = append_observation(h, &changed).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h.last().map(|s| s.period_used), Some(Some(0.31)));
        // Nothing to learn from: not stored.
        assert!(append_observation(h.clone(), &ObservedUsage::empty(t0 + Duration::minutes(5))).is_none());
        assert!(h.windows(2).all(|w| w[0].at <= w[1].at));
        // Out-of-order input is sorted.
        let older = ObservedUsage { period_used: Some(0.2), ..ObservedUsage::empty(t0 - Duration::hours(1)) };
        let h = append_observation(h, &older).unwrap();
        assert!(h.windows(2).all(|w| w[0].at <= w[1].at));
    }

    #[test]
    fn a_legacy_calibration_is_folded_in_once() {
        let t0 = at("2026-10-01T12:00:00Z");
        let legacy = ObservationSample { at: t0, period_used: Some(0.43), window_used: None };
        assert_eq!(fold_legacy_calibration(Vec::new(), Some(legacy)), vec![legacy]);
        assert!(fold_legacy_calibration(Vec::new(), None).is_empty());
        let later = sample(t0 + Duration::hours(1), 0.5);
        let h = fold_legacy_calibration(vec![later], Some(legacy));
        assert_eq!(h, vec![legacy, later], "sorted, oldest first");
        let h = fold_legacy_calibration(vec![legacy, later], Some(legacy));
        assert_eq!(h.len(), 2, "not duplicated");
    }

    #[test]
    fn thinning_keeps_recent_minutes_and_older_ten_minute_steps() {
        let now = at("2026-10-01T12:00:00Z");
        let mut samples = Vec::new();
        // One sample every 10 s for the last 3 hours, plus one ancient sample.
        samples.push(sample(now - Duration::days(10), 0.0));
        let mut t = now - Duration::hours(3);
        while t <= now {
            samples.push(sample(t, 0.5));
            t += Duration::seconds(10);
        }
        let thinned = thin_samples(&samples, now);
        assert_eq!(thinned.last().map(|s| s.at), Some(now), "the newest sample is always kept");
        assert!(thinned.first().unwrap().at >= now - Duration::hours(3), "ancient samples dropped");
        let recent = thinned.iter().filter(|s| now - s.at <= SAMPLE_RECENT).count();
        let old = thinned.len() - recent;
        assert!((118..=121).contains(&recent), "about one per minute over 2h, got {recent}");
        assert!((5..=7).contains(&old), "about one per 10 min over the older hour, got {old}");
        assert!(thinned.windows(2).all(|w| w[1].at - w[0].at >= SAMPLE_SPACING_RECENT));
        assert!(thin_samples(&[], now).is_empty());
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
    fn apply_moves_the_period_and_makes_the_observation_the_truth() {
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
        assert!((l.period_fraction() - 0.4).abs() < 1e-9, "the observation replaces our 10% measurement");
        l.spent_since_observation = 5.0;
        assert!((l.period_fraction() - 0.45).abs() < 1e-9, "plus what was measured since, at the effective budget");
        assert_eq!(l.observed, Some(obs));
    }

    #[test]
    fn apply_enables_the_window_when_the_provider_reports_one() {
        let now = at("2026-10-01T12:00:00Z");
        let mut l = ledger(now);
        l.window_enabled = false;
        let obs = ObservedUsage { window_used: Some(0.25), ..ObservedUsage::empty(now) };
        apply_observed(&mut l, &obs);
        assert!(!l.window_enabled && l.has_window(), "config says no window, but the provider has one");
        assert!((l.window_fraction() - 0.25).abs() < 1e-9);
        assert_eq!(l.anchor_source, AnchorSource::Config, "no reset known: the period stays");

        let mut l = ledger(now);
        let before = l.period;
        let past_reset = ObservedUsage { period_resets_at: Some(now - Duration::hours(1)), ..ObservedUsage::empty(now) };
        apply_observed(&mut l, &past_reset);
        assert_eq!(l.period, before, "a reset in the past does not move the period");
    }
}

#[cfg(test)]
mod properties {
    use super::*;
    use crate::strategies::instant_between;
    use proptest::collection::vec;
    use proptest::prelude::*;

    fn reading() -> impl Strategy<Value = Option<f64>> {
        prop::option::of(prop::sample::select(vec![0.0, 0.1, 0.25, 0.5, 0.99, 1.0, 1.2]))
    }

    /// A history as stored: sorted oldest first, spanning up to twelve days.
    fn history() -> impl Strategy<Value = Vec<ObservationSample>> {
        vec((instant_between(12 * 86_400, 0), reading(), reading()), 0..12).prop_map(|rows| {
            let mut out: Vec<ObservationSample> = rows
                .into_iter()
                .map(|(at, period_used, window_used)| ObservationSample { at, period_used, window_used })
                .collect();
            out.sort_by_key(|s| s.at);
            out
        })
    }

    fn observed() -> impl Strategy<Value = ObservedUsage> {
        (
            reading(),
            prop::option::of(instant_between(86_400, 86_400)),
            reading(),
            prop::option::of(instant_between(86_400, 7 * 86_400)),
            any::<bool>(),
            instant_between(12 * 86_400, 600),
        )
            .prop_map(|(window_used, window_resets_at, period_used, period_resets_at, blocked, observed_at)| {
                ObservedUsage { window_used, window_resets_at, period_used, period_resets_at, blocked, observed_at }
            })
    }

    fn sorted(samples: &[ObservationSample]) -> bool {
        samples.windows(2).all(|w| w[0].at <= w[1].at)
    }

    /// The thinning rule: the older sample of each kept pair sets the spacing.
    fn spaced(samples: &[ObservationSample], now: DateTime<Utc>) -> bool {
        samples.windows(2).all(|w| {
            let spacing = if now - w[0].at <= SAMPLE_RECENT { SAMPLE_SPACING_RECENT } else { SAMPLE_SPACING_OLD };
            w[1].at - w[0].at >= spacing
        })
    }

    proptest! {
        /// Thinning keeps a sorted subset of the input, never anything older
        /// than `SAMPLE_MAX_AGE`, always the newest sample that is young
        /// enough, with consecutive samples at least a minute apart in the
        /// recent two hours and ten minutes apart before that.
        #[test]
        fn thinning_keeps_a_spaced_subset(samples in history(), now in instant_between(0, 86_400)) {
            let kept = thin_samples(&samples, now);
            prop_assert!(kept.len() <= samples.len());
            prop_assert!(sorted(&kept));
            prop_assert!(kept.iter().all(|k| samples.contains(k)), "only input samples are kept");
            prop_assert!(kept.iter().all(|k| now - k.at <= SAMPLE_MAX_AGE));
            prop_assert!(spaced(&kept, now), "{kept:?}");
            if let Some(newest) = samples.last().filter(|s| now - s.at <= SAMPLE_MAX_AGE) {
                prop_assert_eq!(kept.last(), Some(newest), "the newest sample is always kept");
            }
            prop_assert_eq!(&thin_samples(&kept, now), &kept, "thinning is idempotent");
        }

        /// A reading is stored unless it knows nothing or merely repeats the
        /// newest stored reading within a minute. What is stored is the
        /// thinned, sorted history including the new sample when it is the
        /// newest (an out-of-order older reading may be thinned away).
        #[test]
        fn appending_stores_new_information_only(samples in history(), obs in observed()) {
            let sample = ObservationSample::of(&obs);
            let repeat = samples.last().is_some_and(|last| {
                sample.at - last.at < SAMPLE_SPACING_RECENT
                    && last.period_used == sample.period_used
                    && last.window_used == sample.window_used
            });
            match append_observation(samples.clone(), &obs) {
                None => prop_assert!(sample.is_empty() || repeat, "dropped a new reading {sample:?} after {:?}", samples.last()),
                Some(stored) => {
                    prop_assert!(!sample.is_empty() && !repeat);
                    prop_assert!(sorted(&stored));
                    prop_assert!(stored.len() <= samples.len() + 1);
                    prop_assert!(spaced(&stored, sample.at), "{stored:?}");
                    if samples.last().is_none_or(|last| last.at <= sample.at) {
                        prop_assert_eq!(stored.last(), Some(&sample), "the newest reading is kept");
                    }
                    let mut all = samples.clone();
                    all.push(sample);
                    all.sort_by_key(|s| s.at);
                    prop_assert_eq!(&stored, &thin_samples(&all, sample.at));
                }
            }
        }

        /// Folding the legacy calibration in is idempotent, keeps the history
        /// sorted and adds the reading only when no sample sits at its instant.
        #[test]
        fn legacy_calibration_folds_in_once(samples in history(), legacy in prop::option::of((instant_between(12 * 86_400, 0), reading()))) {
            let legacy = legacy.map(|(at, period_used)| ObservationSample { at, period_used, window_used: None });
            let once = fold_legacy_calibration(samples.clone(), legacy);
            prop_assert!(sorted(&once));
            let present = legacy.is_some_and(|l| samples.iter().any(|s| (s.at - l.at).num_seconds().abs() < 1));
            prop_assert_eq!(once.len(), samples.len() + usize::from(legacy.is_some() && !present));
            prop_assert_eq!(&fold_legacy_calibration(once.clone(), legacy), &once, "idempotent");
            prop_assert!(samples.iter().all(|s| once.contains(s)));
        }

        /// The cooldown is the earliest known reset of what is exhausted:
        /// `blocked` looks at both resets, a full window at the window's, a
        /// full period at the period's; nothing exhausted (or no reset known
        /// for it) means no cooldown.
        #[test]
        fn cooldown_follows_the_exhausted_allowance(obs in observed()) {
            let window_out = obs.window_used.is_some_and(|u| u >= 1.0);
            let period_out = obs.period_used.is_some_and(|u| u >= 1.0);
            let mut applicable = Vec::new();
            if obs.blocked || window_out {
                applicable.push(obs.window_resets_at);
            }
            if obs.blocked || period_out {
                applicable.push(obs.period_resets_at);
            }
            let expected = applicable.into_iter().flatten().min();
            prop_assert_eq!(obs.cooldown_until(), expected);
            if !(obs.blocked || window_out || period_out) {
                prop_assert_eq!(obs.cooldown_until(), None);
            }
            prop_assert_eq!(ObservationSample::of(&obs).is_empty(), obs.period_used.is_none() && obs.window_used.is_none());
        }
    }
}
