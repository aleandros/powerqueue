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
        let mut ledger = Ledger::build(cfg, provider, now, &clock, source);
        if let Some(entry) = store.kv_get::<super::overflow::Entry>(&overflow_key(provider))?
            && entry.current(&ledger)
        {
            ledger.overflow.started_at = Some(entry.started_at);
            let rows = store.usage_by_tier(entry.started_at, ledger.period.end)?;
            ledger.overflow.estimated_spend_usd = rows
                .iter()
                .filter(|r| r.tier.provider() == provider)
                .map(|r| r.usage.weighted() * super::ledger::tier_weight(cfg, &r.tier))
                .sum::<f64>()
                .max(0.0)
                * super::overflow::USD_PER_WEIGHTED_TOKEN;
        }
        Ok(ledger)
    }
}

fn overflow_key(provider: Provider) -> String {
    format!("budget.overflow.{provider}")
}

/// Record the first credit allocation in the current period and its event.
/// Only the daemon writes this marker; budget displays remain read-only.
/// Fails on storage errors, so a credit-funded launch must not proceed then.
pub fn enter_overflow(store: &Store, ledger: &Ledger) -> Result<()> {
    let key = overflow_key(ledger.provider);
    if refresh_overflow_entry(store, ledger)?.is_some_and(|entry| entry.current(ledger)) {
        return Ok(());
    }
    let entry = super::overflow::Entry { started_at: ledger.now, period_end: ledger.period.end };
    store
        .kv_set_with_event(
            &key,
            &entry,
            "budget.overflow",
            &format!("{} entered usage credits; spend is estimated from the ledger", ledger.provider),
        )
        .context("record first budget overflow and event")
}

// The daemon persists learned expiry even when it is currently using the
// subscription, so a later partial reading cannot erase or revive allocation.
fn refresh_overflow_entry(store: &Store, ledger: &Ledger) -> Result<Option<super::overflow::Entry>> {
    let key = overflow_key(ledger.provider);
    let Some(entry) = store.kv_get::<super::overflow::Entry>(&key)? else { return Ok(None) };
    let refreshed = entry.refreshed(ledger);
    if refreshed != entry {
        store.kv_set(&key, &refreshed).context("persist learned credit period reset")?;
    }
    Ok(Some(refreshed))
}

/// Detect allowance exhaustion even when all slots are occupied. Running
/// sessions can cross into credits without another task being launched.
pub fn record_overflow(store: &Store, cfg: &BudgetConfig, now: DateTime<Utc>) -> Result<()> {
    for provider in cfg.providers.enabled().into_iter().filter(|p| super::overflow::enabled(cfg, *p)) {
        let ledger = Ledger::load(store, cfg, provider, now)?;
        refresh_overflow_entry(store, &ledger)?;
        if ledger.overflow_reason(cfg, 0.0).is_some() {
            enter_overflow(store, &ledger)?;
        }
    }
    Ok(())
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
    #[expect(
        clippy::items_after_statements,
        reason = "The local helper type is scoped beside the resource or compatibility operation that uses it."
    )]
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
    fn overflow_spend_survives_reload_late_usage_and_window_reset_but_not_period_reset() {
        use crate::domain::{ModelTier, Task, TaskSource, TokenUsage, UsageRecord};
        let store = Store::open_in_memory().unwrap();
        let mut cfg = BudgetConfig::default();
        cfg.providers.claude.allow_overflow = true;
        cfg.providers.claude.period_anchor = Some("2026-10-01T00:00:00Z".into());
        let t0 = at("2026-10-01T12:00:00Z");
        let task = Task::new("overflow", "test", TaskSource::Manual);
        store.insert_task(&task).unwrap();
        let ledger = Ledger::load(&store, &cfg, Provider::Claude, t0).unwrap();
        enter_overflow(&store, &ledger).unwrap();
        enter_overflow(&store, &ledger).unwrap();
        let usage = |id: &str, tier, timestamp| UsageRecord {
            session_id: uuid::Uuid::new_v4(),
            task_id: task.id,
            message_id: id.into(),
            model_id: "test".into(),
            tier,
            usage: TokenUsage { input_tokens: 1_000_000, ..TokenUsage::default() },
            timestamp,
        };
        store.record_usage(&usage("subscription", ModelTier::sonnet(), t0 - Duration::minutes(1))).unwrap();
        let paid = usage("credits", ModelTier::sonnet(), t0 + Duration::minutes(1));
        assert!(store.record_usage(&paid).unwrap());
        assert!(!store.record_usage(&paid).unwrap());
        store.record_usage(&usage("other provider", ModelTier::new("gpt-6-astra"), t0 + Duration::minutes(1))).unwrap();
        let later = t0 + Duration::hours(6);
        let loaded = Ledger::load(&store, &cfg, Provider::Claude, later).unwrap();
        assert!((loaded.overflow.estimated_spend_usd - 3.0).abs() < 1e-9);
        assert_eq!(loaded.overflow.started_at, Some(t0));
        assert_eq!(loaded.overflow.reserved_usd, 0.0);
        // Late transcript tails are charged by timestamp, not by polling order.
        store.record_usage(&usage("late", ModelTier::opus(), t0 + Duration::minutes(2))).unwrap();
        cfg.providers.claude.allow_overflow = false;
        let reloaded = Ledger::load(&store, &cfg, Provider::Claude, later).unwrap();
        assert!((reloaded.overflow.estimated_spend_usd - 12.0).abs() < 1e-9);
        assert!(!reloaded.overflow.enabled);
        assert_eq!(store.recent_events(100).unwrap().iter().filter(|e| e.kind == "budget.overflow").count(), 1);
        let next = Ledger::load(&store, &cfg, Provider::Claude, ledger.period.end).unwrap();
        assert_eq!(next.overflow.started_at, None);
        assert_eq!(next.overflow.estimated_spend_usd, 0.0);
        enter_overflow(&store, &next).unwrap();
        assert_eq!(store.recent_events(100).unwrap().iter().filter(|e| e.kind == "budget.overflow").count(), 2);
    }

    #[test]
    fn overflow_anchor_learning_does_not_reset_the_cap_mid_period() {
        let store = Store::open_in_memory().unwrap();
        let mut cfg = BudgetConfig::default();
        cfg.providers.claude.allow_overflow = true;
        cfg.providers.claude.period_anchor = Some("2026-10-01T00:00:00Z".into());
        let start = at("2026-10-07T12:00:00Z");
        let initial = Ledger::load(&store, &cfg, Provider::Claude, start).unwrap();
        enter_overflow(&store, &initial).unwrap();
        let actual_reset = initial.period.end + Duration::days(1);
        save_observed(
            &store,
            Provider::Claude,
            &ObservedUsage { period_used: Some(1.0), period_resets_at: Some(actual_reset), ..ObservedUsage::empty(start) },
        )
        .unwrap();
        let learned = Ledger::load(&store, &cfg, Provider::Claude, initial.period.end).unwrap();
        assert_eq!(learned.overflow.started_at, Some(start));
        enter_overflow(&store, &learned).unwrap();
        assert_eq!(store.recent_events(10).unwrap().len(), 1);
        let reset = Ledger::load(&store, &cfg, Provider::Claude, actual_reset).unwrap();
        assert_eq!(reset.overflow.started_at, None);
    }

    #[test]
    fn partial_readings_preserve_learned_overflow_expiry_without_another_credit_launch() {
        let store = Store::open_in_memory().unwrap();
        let mut cfg = BudgetConfig::default();
        cfg.providers.claude.allow_overflow = true;
        cfg.providers.claude.period_anchor = Some("2026-10-01T00:00:00Z".into());
        let start = at("2026-10-07T12:00:00Z");
        let initial = Ledger::load(&store, &cfg, Provider::Claude, start).unwrap();
        enter_overflow(&store, &initial).unwrap();
        let actual_reset = initial.period.end + Duration::days(1);
        save_observed(
            &store,
            Provider::Claude,
            &ObservedUsage { period_used: Some(0.2), period_resets_at: Some(actual_reset), ..ObservedUsage::empty(start) },
        )
        .unwrap();
        // Even without current exhaustion, a tick preserves the learned date.
        record_overflow(&store, &cfg, start + Duration::minutes(1)).unwrap();
        save_observed(
            &store,
            Provider::Claude,
            &ObservedUsage { window_used: Some(0.1), ..ObservedUsage::empty(initial.period.end) },
        )
        .unwrap();
        let partial = Ledger::load(&store, &cfg, Provider::Claude, initial.period.end).unwrap();
        assert_eq!(partial.overflow.started_at, Some(start));
        let reset = Ledger::load(&store, &cfg, Provider::Claude, actual_reset).unwrap();
        assert_eq!(reset.overflow.started_at, None);
        assert_eq!(store.recent_events(10).unwrap().len(), 1);
    }

    #[test]
    fn overflow_expires_at_an_earlier_learned_reset_and_stays_expired() {
        let store = Store::open_in_memory().unwrap();
        let mut cfg = BudgetConfig::default();
        cfg.providers.claude.allow_overflow = true;
        cfg.providers.claude.period_anchor = Some("2026-10-01T00:00:00Z".into());
        let start = at("2026-10-03T12:00:00Z");
        let initial = Ledger::load(&store, &cfg, Provider::Claude, start).unwrap();
        enter_overflow(&store, &initial).unwrap();
        let actual_reset = start + Duration::days(1);
        save_observed(
            &store,
            Provider::Claude,
            &ObservedUsage { period_used: Some(1.0), period_resets_at: Some(actual_reset), ..ObservedUsage::empty(start) },
        )
        .unwrap();
        let before_reset = Ledger::load(&store, &cfg, Provider::Claude, actual_reset - Duration::seconds(1)).unwrap();
        assert_eq!(before_reset.overflow.started_at, Some(start));
        let reset = Ledger::load(&store, &cfg, Provider::Claude, actual_reset).unwrap();
        assert_eq!(reset.overflow.started_at, None);
        save_observed(
            &store,
            Provider::Claude,
            &ObservedUsage {
                period_used: Some(0.1),
                period_resets_at: Some(actual_reset + Duration::days(7)),
                ..ObservedUsage::empty(actual_reset)
            },
        )
        .unwrap();
        let refreshed = Ledger::load(&store, &cfg, Provider::Claude, actual_reset + Duration::minutes(1)).unwrap();
        assert_eq!(refreshed.overflow.started_at, None);
    }

    #[test]
    fn overflow_recording_detects_running_session_exhaustion_and_is_opt_in() {
        let store = Store::open_in_memory().unwrap();
        let mut cfg = BudgetConfig::default();
        let now = at("2026-10-01T12:00:00Z");
        save_observed(&store, Provider::Claude, &ObservedUsage { period_used: Some(1.0), ..ObservedUsage::empty(now) }).unwrap();
        record_overflow(&store, &cfg, now).unwrap();
        assert_eq!(store.recent_events(10).unwrap().len(), 0);
        cfg.providers.claude.allow_overflow = true;
        record_overflow(&store, &cfg, now).unwrap();
        record_overflow(&store, &cfg, now + Duration::minutes(1)).unwrap();
        assert_eq!(store.recent_events(10).unwrap().len(), 1);
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
        assert_eq!((load_observations(&store, Provider::Claude).unwrap()).len(), 0);
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
        save_observed(&store, Provider::Claude, &ObservedUsage { observed_at: t0 + Duration::seconds(20), ..base }).unwrap();
        assert_eq!(load_observations(&store, Provider::Claude).unwrap().len(), 1);
        save_observed(&store, Provider::Claude, &ObservedUsage { observed_at: t0 + Duration::minutes(2), ..base }).unwrap();
        assert_eq!(load_observations(&store, Provider::Claude).unwrap().len(), 2);
        // A changed reading 10s later replaces the one just before it (one per minute is kept).
        let changed =
            ObservedUsage { period_used: Some(0.31), observed_at: t0 + Duration::minutes(2) + Duration::seconds(10), ..base };
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
