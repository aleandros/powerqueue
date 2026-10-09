//! The store-facing side of the budget: everything here reads or writes
//! SQLite and hands plain rows to the pure builders in [`super::ledger`] and
//! [`super::probe`]. Nothing in those modules touches the store.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::config::BudgetConfig;
use crate::domain::Provider;
use crate::store::Store;

use super::ledger::{Ledger, LedgerSource, Ledgers, calibration_key, resolve_clock};
use super::probe::{
    ObservationSample, ObservedUsage, append_observation, fold_legacy_calibration, observations_key, observed_key,
};

impl Ledger {
    /// Aggregate usage rows into a ledger for one provider.
    ///
    /// Reads the latest observation (kv `budget.observed.<provider>`), lets
    /// [`resolve_clock`] place the period and window, reads the `usage` rows
    /// of those ranges plus the observation history (kv
    /// `budget.observations.<provider>`), and hands everything to
    /// [`Ledger::build`]. Fails only when the database cannot be read.
    pub fn load(store: &Store, cfg: &BudgetConfig, provider: Provider, now: DateTime<Utc>) -> Result<Ledger> {
        let observed = load_observed(store, provider)?;
        let clock = resolve_clock(cfg.provider(provider), observed.as_ref(), now);
        let period = clock.current_period(now);
        let window = clock.current_window(now);
        let period_rows = store.usage_by_tier(period.start, period.end).context("read period usage")?;
        let window_rows = store.usage_by_tier(window.start, window.end).context("read window usage")?;
        let history = load_observations(store, provider)?;
        let minute_rows = store.usage_by_minute(period.start, period.end).context("read usage per minute")?;
        let since_observation_rows = match &observed {
            Some(obs) if period.end > obs.observed_at => {
                store.usage_by_tier(obs.observed_at, period.end).context("read usage since the observation")?
            }
            _ => Vec::new(),
        };
        let source = LedgerSource { observed, history, period_rows, window_rows, minute_rows, since_observation_rows };
        Ok(Ledger::build(cfg, provider, now, &clock, source))
    }
}

impl Ledgers {
    /// Load a ledger for every enabled provider. Fails when any of them
    /// cannot be read from the database.
    pub fn load(store: &Store, cfg: &BudgetConfig, now: DateTime<Utc>) -> Result<Ledgers> {
        let mut by_provider = std::collections::BTreeMap::new();
        for provider in cfg.providers.enabled() {
            let ledger = Ledger::load(store, cfg, provider, now).with_context(|| format!("load {provider} ledger"))?;
            by_provider.insert(provider, ledger);
        }
        Ok(Ledgers { by_provider })
    }
}

/// The stored observation history of a provider, oldest first. A pre-0.7
/// calibration (kv `budget.calibration.<p>`: `observed_fraction` at `at`)
/// is folded in as one more reading when the history has none at that
/// instant, so an upgrade keeps the reading it had. Fails only when the
/// database cannot be read or the stored JSON is unreadable.
pub fn load_observations(store: &Store, provider: Provider) -> Result<Vec<ObservationSample>> {
    let samples = store
        .kv_get::<Vec<ObservationSample>>(&observations_key(provider))
        .with_context(|| format!("read observation history of {provider}"))?
        .unwrap_or_default();
    #[derive(Deserialize)]
    struct Legacy {
        observed_fraction: f64,
        at: DateTime<Utc>,
    }
    let legacy = store.kv_get::<Legacy>(&calibration_key(provider)).ok().flatten().map(|l| ObservationSample {
        at: l.at,
        period_used: Some(l.observed_fraction),
        window_used: None,
    });
    Ok(fold_legacy_calibration(samples, legacy))
}

/// Append a sample to the history and thin it ([`append_observation`]). A
/// sample that only repeats the newest reading within
/// [`super::probe::SAMPLE_SPACING_RECENT`] is not stored (the status line
/// fires after every response).
pub fn record_observation(store: &Store, provider: Provider, obs: &ObservedUsage) -> Result<()> {
    if ObservationSample::of(obs).is_empty() {
        // Most status-line payloads carry no usage: nothing to read or write.
        return Ok(());
    }
    let samples = load_observations(store, provider)?;
    match append_observation(samples, obs) {
        Some(thinned) => store
            .kv_set(&observations_key(provider), &thinned)
            .with_context(|| format!("store observation history of {provider}")),
        None => Ok(()),
    }
}

/// The latest observation for a provider, if any. Fails only when the
/// database cannot be read or the stored JSON is unreadable.
pub fn load_observed(store: &Store, provider: Provider) -> Result<Option<ObservedUsage>> {
    store.kv_get(&observed_key(provider)).with_context(|| format!("read observed usage of {provider}"))
}

/// Store a provider's observation (latest wins) and append it to the
/// history ([`record_observation`]).
pub fn save_observed(store: &Store, provider: Provider, observed: &ObservedUsage) -> Result<()> {
    store.kv_set(&observed_key(provider), observed).with_context(|| format!("store observed usage of {provider}"))?;
    record_observation(store, provider, observed)
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::budget::probe::{NoProbe, UsageProbe};

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn kv_round_trip() {
        let store = Store::open_in_memory().unwrap();
        assert!(load_observed(&store, Provider::Codex).unwrap().is_none());
        let obs = ObservedUsage { period_used: Some(0.3), ..ObservedUsage::empty(at("2026-10-01T12:00:00Z")) };
        save_observed(&store, Provider::Codex, &obs).unwrap();
        assert_eq!(load_observed(&store, Provider::Codex).unwrap(), Some(obs.clone()));
        assert_eq!(load_observations(&store, Provider::Codex).unwrap(), vec![ObservationSample::of(&obs)]);
        assert!(load_observed(&store, Provider::Claude).unwrap().is_none());
        assert!(load_observations(&store, Provider::Claude).unwrap().is_empty());
        assert_eq!(observed_key(Provider::Gemini), "budget.observed.gemini");
        assert_eq!(observations_key(Provider::Gemini), "budget.observations.gemini");
        assert!(NoProbe(Provider::Gemini).probe().unwrap().is_none());
        assert_eq!(NoProbe(Provider::Gemini).provider(), Provider::Gemini);
    }

    #[test]
    fn history_skips_repeats_and_empty_readings_and_stays_sorted() {
        let store = Store::open_in_memory().unwrap();
        let t0 = at("2026-10-01T12:00:00Z");
        let base = ObservedUsage { period_used: Some(0.3), window_used: Some(0.1), ..ObservedUsage::empty(t0) };
        save_observed(&store, Provider::Claude, &base).unwrap();
        // Same reading 20s later: not stored. Same reading 2 min later: stored.
        save_observed(&store, Provider::Claude, &ObservedUsage { observed_at: t0 + Duration::seconds(20), ..base.clone() })
            .unwrap();
        assert_eq!(load_observations(&store, Provider::Claude).unwrap().len(), 1);
        save_observed(&store, Provider::Claude, &ObservedUsage { observed_at: t0 + Duration::minutes(2), ..base.clone() })
            .unwrap();
        assert_eq!(load_observations(&store, Provider::Claude).unwrap().len(), 2);
        // A changed reading 10s later replaces the one just before it (one per minute is kept).
        let changed = ObservedUsage {
            period_used: Some(0.31),
            observed_at: t0 + Duration::minutes(2) + Duration::seconds(10),
            ..base.clone()
        };
        save_observed(&store, Provider::Claude, &changed).unwrap();
        let h = load_observations(&store, Provider::Claude).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h.last().map(|s| s.period_used), Some(Some(0.31)));
        // Nothing to learn from: not stored.
        save_observed(&store, Provider::Claude, &ObservedUsage::empty(t0 + Duration::minutes(5))).unwrap();
        let h = load_observations(&store, Provider::Claude).unwrap();
        assert_eq!(h.len(), 2);
        assert!(h.windows(2).all(|w| w[0].at <= w[1].at));
    }

    #[test]
    fn a_pre_0_7_calibration_counts_as_a_reading() {
        let store = Store::open_in_memory().unwrap();
        let t0 = at("2026-10-01T12:00:00Z");
        store
            .kv_set(
                &calibration_key(Provider::Claude),
                &serde_json::json!({ "observed_fraction": 0.43, "at": t0, "measured_fraction": 0.57 }),
            )
            .unwrap();
        let h = load_observations(&store, Provider::Claude).unwrap();
        assert_eq!(h, vec![ObservationSample { at: t0, period_used: Some(0.43), window_used: None }]);
        // Later readings come after it; the legacy one is not duplicated.
        let obs = ObservedUsage { period_used: Some(0.5), ..ObservedUsage::empty(t0 + Duration::hours(1)) };
        save_observed(&store, Provider::Claude, &obs).unwrap();
        let h = load_observations(&store, Provider::Claude).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].at, t0);
        assert_eq!(h[1].period_used, Some(0.5));
    }
}
