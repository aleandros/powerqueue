//! Usage periods: a fixed-length period anchored at a known reset instant,
//! and a rolling window.

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::config::ProviderBudget;

/// A half-open interval `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Period {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl Period {
    pub fn contains(&self, t: DateTime<Utc>) -> bool {
        t >= self.start && t < self.end
    }
    pub fn len(&self) -> Duration {
        self.end - self.start
    }
    /// 0.0 at start, 1.0 at end (clamped).
    pub fn elapsed_fraction(&self, now: DateTime<Utc>) -> f64 {
        let total = self.len().num_seconds().max(1) as f64;
        ((now - self.start).num_seconds() as f64 / total).clamp(0.0, 1.0)
    }
    pub fn remaining(&self, now: DateTime<Utc>) -> Duration {
        (self.end - now).max(Duration::zero())
    }
}

/// Where the period anchor came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AnchorSource {
    /// `budget.providers.<p>.period_anchor`.
    Config,
    /// A usage probe reported the next reset.
    Observed,
    /// Nothing known: Monday 00:00 UTC of the current week.
    #[default]
    Default,
}

/// Computes periods from one provider's budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodClock {
    pub period: Duration,
    /// Zero when the provider has no rolling window (`window_hours = 0`).
    pub window: Duration,
    /// A known period start; periods repeat every `period` before and after it.
    pub anchor: DateTime<Utc>,
    pub anchor_source: AnchorSource,
}

impl PeriodClock {
    /// Build from a provider's budget. Without an anchor we default to Monday
    /// 00:00 UTC of the current week, which is wrong for most accounts but
    /// stable; `doctor` nags until `budget set-reset` is used or a probe
    /// learns the reset.
    ///
    /// An anchor that does not parse as RFC 3339 is ignored (config validation
    /// reports it separately) and the Monday default is used instead. The
    /// period length is clamped to at least one hour so the clock never
    /// divides by zero; `window_hours = 0` is kept as "no window".
    pub fn from_provider(budget: &ProviderBudget, now: DateTime<Utc>) -> Self {
        let configured = budget
            .period_anchor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s.trim()).ok())
            .map(|d| d.with_timezone(&Utc));
        let (anchor, anchor_source) = match configured {
            Some(a) => (a, AnchorSource::Config),
            None => (most_recent_monday(now), AnchorSource::Default),
        };
        Self {
            period: Duration::hours(budget.period_hours.max(1) as i64),
            window: Duration::hours(budget.window_hours as i64),
            anchor,
            anchor_source,
        }
    }

    /// Replace the anchor with a reset instant a probe observed. Any instant
    /// on the period grid works because periods repeat around the anchor.
    pub fn with_observed_anchor(mut self, resets_at: DateTime<Utc>) -> Self {
        self.anchor = resets_at;
        self.anchor_source = AnchorSource::Observed;
        self
    }

    /// False when the provider has no rolling window.
    pub fn has_window(&self) -> bool {
        self.window > Duration::zero()
    }

    /// The period containing `now`. Works before and after the anchor: the
    /// start is `anchor + floor((now - anchor) / period) × period`.
    pub fn current_period(&self, now: DateTime<Utc>) -> Period {
        let period_secs = self.period.num_seconds().max(1);
        let delta = (now - self.anchor).num_seconds();
        let index = delta.div_euclid(period_secs);
        let start = self.anchor + Duration::seconds(index * period_secs);
        Period { start, end: start + Duration::seconds(period_secs) }
    }

    /// The rolling window ending at `now` (empty, `[now, now)`, without a window).
    pub fn current_window(&self, now: DateTime<Utc>) -> Period {
        Period { start: now - self.window, end: now }
    }
}

/// Monday 00:00 UTC on or before `now`.
fn most_recent_monday(now: DateTime<Utc>) -> DateTime<Utc> {
    let days_since_monday = now.weekday().num_days_from_monday() as i64;
    let date = now.date_naive() - Duration::days(days_since_monday);
    Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight is a valid time"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provider;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn budget_with_anchor(anchor: Option<&str>) -> ProviderBudget {
        ProviderBudget { period_anchor: anchor.map(str::to_string), ..ProviderBudget::defaults_for(Provider::Claude) }
    }

    #[test]
    fn default_anchor_is_most_recent_monday_midnight() {
        // 2026-10-01 is a Thursday.
        let now = at("2026-10-01T15:30:00Z");
        let clock = PeriodClock::from_provider(&budget_with_anchor(None), now);
        assert_eq!(clock.anchor, at("2026-09-28T00:00:00Z"));
        assert_eq!(clock.anchor_source, AnchorSource::Default);
        assert_eq!(clock.period, Duration::hours(24 * 7));
        assert_eq!(clock.window, Duration::hours(5));
        assert!(clock.has_window());
        let p = clock.current_period(now);
        assert_eq!(p.start, at("2026-09-28T00:00:00Z"));
        assert_eq!(p.end, at("2026-10-05T00:00:00Z"));
        assert!(p.contains(now));
    }

    #[test]
    fn monday_itself_is_its_own_anchor() {
        let now = at("2026-09-28T00:00:00Z");
        let clock = PeriodClock::from_provider(&budget_with_anchor(None), now);
        assert_eq!(clock.anchor, now);
        assert_eq!(clock.current_period(now).start, now);
    }

    #[test]
    fn explicit_anchor_in_the_past() {
        let now = at("2026-10-01T15:30:00Z");
        let clock = PeriodClock::from_provider(&budget_with_anchor(Some("2026-09-02T09:00:00+02:00")), now);
        assert_eq!(clock.anchor, at("2026-09-02T07:00:00Z"));
        assert_eq!(clock.anchor_source, AnchorSource::Config);
        let p = clock.current_period(now);
        // 2026-09-02T07:00Z + 4 weeks = 2026-09-30T07:00Z.
        assert_eq!(p.start, at("2026-09-30T07:00:00Z"));
        assert_eq!(p.end, at("2026-10-07T07:00:00Z"));
        assert!(p.contains(now));
    }

    #[test]
    fn explicit_anchor_in_the_future_uses_floor_division() {
        let now = at("2026-10-01T15:30:00Z");
        let clock = PeriodClock::from_provider(&budget_with_anchor(Some("2026-10-20T12:00:00Z")), now);
        let p = clock.current_period(now);
        assert_eq!(p.start, at("2026-09-29T12:00:00Z"));
        assert_eq!(p.end, at("2026-10-06T12:00:00Z"));
        assert!(p.contains(now));
    }

    #[test]
    fn observed_anchor_overrides_the_configured_one() {
        let now = at("2026-10-01T15:30:00Z");
        let clock = PeriodClock::from_provider(&budget_with_anchor(Some("2026-10-20T12:00:00Z")), now)
            .with_observed_anchor(at("2026-10-03T08:00:00Z"));
        assert_eq!(clock.anchor_source, AnchorSource::Observed);
        let p = clock.current_period(now);
        assert_eq!(p.start, at("2026-09-26T08:00:00Z"));
        assert_eq!(p.end, at("2026-10-03T08:00:00Z"));
    }

    #[test]
    fn period_boundaries_are_half_open() {
        let now = at("2026-10-01T00:00:00Z");
        let clock = PeriodClock::from_provider(&budget_with_anchor(Some("2026-10-01T00:00:00Z")), now);
        let p = clock.current_period(now);
        assert_eq!(p.start, now);
        assert!(p.contains(now));
        assert!(!p.contains(p.end));
        let before = clock.current_period(now - Duration::seconds(1));
        assert_eq!(before.end, now);
        let after = clock.current_period(p.end);
        assert_eq!(after.start, p.end);
    }

    #[test]
    fn invalid_anchor_falls_back_to_monday() {
        let now = at("2026-10-01T15:30:00Z");
        let clock = PeriodClock::from_provider(&budget_with_anchor(Some("not a date")), now);
        assert_eq!(clock.anchor, at("2026-09-28T00:00:00Z"));
        assert_eq!(clock.anchor_source, AnchorSource::Default);
    }

    #[test]
    fn elapsed_fraction_and_remaining() {
        let start = at("2026-10-01T00:00:00Z");
        let p = Period { start, end: start + Duration::hours(10) };
        assert_eq!(p.elapsed_fraction(start), 0.0);
        assert!((p.elapsed_fraction(start + Duration::hours(2)) - 0.2).abs() < 1e-9);
        assert_eq!(p.elapsed_fraction(start + Duration::hours(20)), 1.0);
        assert_eq!(p.elapsed_fraction(start - Duration::hours(1)), 0.0);
        assert_eq!(p.remaining(start + Duration::hours(4)), Duration::hours(6));
        assert_eq!(p.remaining(start + Duration::hours(40)), Duration::zero());
        assert_eq!(p.len(), Duration::hours(10));
    }

    #[test]
    fn window_ends_now() {
        let now = at("2026-10-01T15:30:00Z");
        let clock = PeriodClock::from_provider(&budget_with_anchor(None), now);
        let w = clock.current_window(now);
        assert_eq!(w.end, now);
        assert_eq!(w.start, now - Duration::hours(5));
    }

    #[test]
    fn zero_period_is_clamped_and_zero_window_means_no_window() {
        let now = at("2026-10-01T15:30:00Z");
        let budget = ProviderBudget { period_hours: 0, window_hours: 0, ..ProviderBudget::defaults_for(Provider::Claude) };
        let clock = PeriodClock::from_provider(&budget, now);
        assert_eq!(clock.period, Duration::hours(1));
        assert_eq!(clock.window, Duration::zero());
        assert!(!clock.has_window());
        assert!(clock.current_period(now).contains(now));
        let w = clock.current_window(now);
        assert_eq!(w.start, w.end, "no window: an empty range");
    }
}
