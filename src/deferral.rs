//! Explicit launch holds. Source timestamps are absolute so polling never slides them forward.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{Task, TaskSource};

/// A user-selected eligibility time, kept after expiry for inspection and audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deferral {
    pub until: DateTime<Utc>,
    pub reason: Option<String>,
}

/// Parse an RFC 3339 instant with mandatory offset, or a positive duration
/// relative to `now`. Returns an error for ambiguous local times or overflow.
pub fn parse_time(value: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    if let Ok(at) = absolute_time(value) {
        return Ok(at);
    }
    let duration = humantime::parse_duration(value.trim())
        .context("use an RFC 3339 timestamp with Z or an explicit offset, or a duration such as 2h")?;
    if duration.is_zero() {
        bail!("deferral duration must be positive");
    }
    let duration = chrono::Duration::from_std(duration).context("deferral duration is too large")?;
    now.checked_add_signed(duration).context("deferral timestamp is out of range")
}

fn absolute_time(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.trim())
        .map(|at| at.with_timezone(&Utc))
        .context("Powerqueue-Defer-Until must be an RFC 3339 timestamp with Z or an explicit offset")
}

/// Read reserved, standalone description lines. Duplicate or invalid directives
/// fail closed; relative times are forbidden in source issues. Other text is ignored.
pub fn parse_description(description: &str) -> Result<Option<Deferral>> {
    let mut until = None;
    let mut reason = None;
    for line in description.lines().map(str::trim) {
        let Some((key, value)) = line.split_once(':') else { continue };
        if key.eq_ignore_ascii_case("Powerqueue-Defer-Until") {
            if until.is_some() {
                bail!("duplicate Powerqueue-Defer-Until line");
            }
            until = Some(absolute_time(value)?);
        } else if key.eq_ignore_ascii_case("Powerqueue-Defer-Reason") {
            if reason.is_some() {
                bail!("duplicate Powerqueue-Defer-Reason line");
            }
            reason = Some(value.trim().to_string());
        }
    }
    if until.is_none() && reason.is_some() {
        bail!("Powerqueue-Defer-Reason requires Powerqueue-Defer-Until");
    }
    Ok(until.map(|until| Deferral { until, reason: reason.filter(|r| !r.is_empty()) }))
}

impl Task {
    /// Source hold from the persisted issue description; manual descriptions are ordinary text.
    /// Malformed source directives return an error and prevent future launches.
    pub fn source_deferral(&self) -> Result<Option<Deferral>> {
        if matches!(self.source, TaskSource::Manual) { Ok(None) } else { parse_description(&self.description) }
    }

    /// Both local and source holds must have expired. Does not grant admission,
    /// release pauses, or bypass any other scheduler restriction.
    pub fn deferral_allows(&self, now: DateTime<Utc>) -> bool {
        self.deferral.as_ref().is_none_or(|d| d.until <= now)
            && self.source_deferral().is_ok_and(|d| d.is_none_or(|d| d.until <= now))
    }

    /// Human-readable local/source eligibility and reasons, including expired holds.
    pub fn deferral_summary(&self, now: DateTime<Utc>) -> Option<String> {
        let mut parts = Vec::new();
        let describe = |origin: &str, d: &Deferral| {
            format!(
                "{origin} {} until {}{}{}",
                if d.until > now { "deferred" } else { "expired" },
                d.until.to_rfc3339(),
                d.reason.as_ref().map_or("", |_| ": "),
                d.reason.as_deref().unwrap_or("")
            )
        };
        if let Some(d) = &self.deferral {
            parts.push(describe("local", d));
        }
        match self.source_deferral() {
            Ok(Some(d)) => parts.push(describe(self.source.kind(), &d)),
            Err(error) => parts.push(format!("source deferral invalid (launch held): {error}")),
            Ok(None) => {}
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategies::origin;

    #[test]
    fn explicit_offsets_durations_and_invalid_times() {
        let now = origin();
        assert_eq!(parse_time("2026-10-01T06:00:00-06:00", now).unwrap(), now);
        assert_eq!(parse_time("2h 30m", now).unwrap(), now + chrono::Duration::minutes(150));
        for bad in ["tomorrow", "2026-10-20", "2026-10-20T09:00:00", "0s", "-2h", "999999999999999999999999d"] {
            assert!(parse_time(bad, now).is_err(), "accepted {bad}");
        }
        assert!(parse_time("1s", DateTime::<Utc>::MAX_UTC).is_err());
        assert!(parse_description("Powerqueue-Defer-Until: 2h").is_err());
        assert!(parse_description("Powerqueue-Defer-Reason: wait").is_err());
        assert!(
            parse_description("Powerqueue-Defer-Until: 2026-10-01T12:00:00Z\nPowerqueue-Defer-Until: 2026-10-02T12:00:00Z")
                .is_err()
        );
        assert!(
            parse_description(
                "Powerqueue-Defer-Until: 2026-10-01T12:00:00Z\nPowerqueue-Defer-Reason: a\nPowerqueue-Defer-Reason: b"
            )
            .is_err()
        );
        assert_eq!(parse_description("ordinary prose about postponing things").unwrap(), None);
    }
}

#[cfg(test)]
mod properties {
    use super::*;
    use crate::budget::Estimator;
    use crate::scheduler::{
        commands,
        launch::{LaunchContext, LaunchPlanner},
        lifecycle::pick_next,
    };
    use crate::strategies::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn offset_roundtrip_is_the_same_instant(at in instant(), offset in -23i32..24) {
            let zone = chrono::FixedOffset::east_opt(offset * 3600).unwrap();
            prop_assert_eq!(parse_time(&at.with_timezone(&zone).to_rfc3339(), origin()).unwrap(), at);
        }

        #[test]
        fn transition_changes_only_local_hold(mut task in task(), hold in prop::option::of(deferral())) {
            let before = task.clone();
            let result = commands::on_defer(&mut task, hold.clone());
            if before.state == crate::domain::TaskState::Starting {
                prop_assert!(result.is_err());
                prop_assert_eq!(task, before);
            } else {
                let effects = result.unwrap();
                let mut expected = before.clone();
                expected.deferral = hold;
                prop_assert_eq!(&task, &expected);
                prop_assert_eq!(effects.is_empty(), before == expected);
                prop_assert!(commands::on_defer(&mut task, expected.deferral).unwrap().is_empty());
            }
        }

        #[test]
        fn exact_expiry_restores_only_original_eligibility(mut task in task(), at in instant()) {
            let baseline = pick_next(std::slice::from_ref(&task), at).is_some();
            task.deferral = Some(Deferral { until: at, reason: None });
            prop_assert!(pick_next(std::slice::from_ref(&task), at - chrono::Duration::nanoseconds(1)).is_none());
            prop_assert_eq!(pick_next(std::slice::from_ref(&task), at).is_some(), baseline);
        }

        #[test]
        fn independent_holds_require_both_expiries(local in deferral(), source in deferral(), now in instant()) {
            let mut task = Task::new("L-1", "test", TaskSource::Linear {
                issue_id: "id".into(), identifier: "L-1".into(), url: "url".into(), team_key: "L".into(),
            });
            task.deferral = Some(local.clone());
            task.description = format!("Powerqueue-Defer-Until: {}", source.until.to_rfc3339());
            prop_assert_eq!(task.deferral_allows(now), now >= local.until && now >= source.until);
            commands::on_defer(&mut task, None).unwrap();
            prop_assert_eq!(task.deferral_allows(now), now >= source.until);
            task.description.clear();
            prop_assert!(task.deferral_allows(now));
        }

        #[test]
        fn expiry_preserves_budget_cooldowns_priority_and_capacity(
            tasks in proptest::collection::vec(task(), 1..8), budget in budget_config(),
            ledgers in ledgers(), limits in rate_limits(), slots in 0u32..5,
        ) {
            let now = origin();
            let rules = crate::priority::PriorityRules::default();
            let ctx = LaunchContext { budget: &budget, rules: &rules, rate_limits: &limits };
            let mut expired = tasks.clone();
            for task in &mut expired { task.deferral = Some(Deferral { until: now, reason: None }); }
            let mut baseline = LaunchPlanner::new(Estimator::from_summaries(&[]), ledgers.clone(), tasks, slots);
            let mut actual = LaunchPlanner::new(Estimator::from_summaries(&[]), ledgers, expired, slots);
            loop {
                let left = baseline.next(ctx, now);
                let right = actual.next(ctx, now);
                prop_assert_eq!(left.as_ref().map(|c| (c.task.id, &c.decision)), right.as_ref().map(|c| (c.task.id, &c.decision)));
                match (left, right) {
                    (Some(a), Some(b)) => { baseline.started(&budget, &a); actual.started(&budget, &b); }
                    _ => break,
                }
            }
            prop_assert_eq!(baseline.ledgers(), actual.ledgers());
        }
    }
}
