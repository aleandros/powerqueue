//! Claude usage "probe": no network call. Each task's `settings.json` sets a
//! `statusLine` command (`powerqueue hook … --event StatusLine`); Claude Code
//! runs it after responses with JSON on stdin that includes
//! `rate_limits.five_hour` / `rate_limits.seven_day` (`used_percentage`
//! 0–100, `resets_at` unix seconds; claude.ai Pro/Max only, after the first
//! response). The hook stores that as kv `budget.observed.claude`
//! ([`parse_status_line`]); [`ClaudeProbe`] just reads it back.

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::budget::probe::{ObservedUsage, UsageProbe, load_observed};
use crate::domain::{ModelTier, Provider, TaskId};
use crate::store::Store;

use super::epoch_secs;

/// Returns the observation the status-line hook stored last.
#[derive(Debug, Clone)]
pub struct ClaudeProbe {
    pub store: Store,
}

impl ClaudeProbe {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

impl UsageProbe for ClaudeProbe {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    /// The stored observation, `Ok(None)` before any status line arrived.
    /// Fails only when the database cannot be read.
    fn probe(&self) -> Result<Option<ObservedUsage>> {
        load_observed(&self.store, Provider::Claude)
    }
}

/// Hook event name the status-line command passes (`--event StatusLine`).
pub const STATUS_LINE_EVENT: &str = "StatusLine";

/// The `statusLine` entry for a task's `settings.json`: a command running
/// `<powerqueue> hook --task <id> --session <sid> --event StatusLine` (the
/// binary path shell-quoted, as for the hooks).
pub fn status_line_setting(powerqueue_bin: &Path, task_id: TaskId, session_id: uuid::Uuid) -> serde_json::Value {
    let bin = crate::tmux::shell_quote(&powerqueue_bin.to_string_lossy());
    serde_json::json!({
        "type": "command",
        "command": format!("{bin} hook --task {task_id} --session {session_id} --event {STATUS_LINE_EVENT}"),
    })
}

/// Decode the status-line JSON's `rate_limits` into an [`ObservedUsage`]
/// taken at `now`: `five_hour` is the window, `seven_day` the period.
/// `None` when the payload has no rate limits (API-key users, before the
/// first response) or neither bucket carries a percentage.
pub fn parse_status_line(payload: &serde_json::Value, now: DateTime<Utc>) -> Option<ObservedUsage> {
    let limits = payload.get("rate_limits").filter(|v| v.is_object())?;
    let bucket = |name: &str| -> Option<(f64, Option<DateTime<Utc>>)> {
        let b = limits.get(name)?;
        let used = b.get("used_percentage").or_else(|| b.get("utilization")).and_then(|v| v.as_f64())?;
        let resets = b.get("resets_at").and_then(|v| {
            epoch_secs(v).or_else(|| v.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc)))
        });
        Some(((used / 100.0).max(0.0), resets))
    };
    let window = bucket("five_hour");
    let period = bucket("seven_day");
    if window.is_none() && period.is_none() {
        return None;
    }
    Some(ObservedUsage {
        window_used: window.map(|w| w.0),
        window_resets_at: window.and_then(|w| w.1),
        period_used: period.map(|p| p.0),
        period_resets_at: period.and_then(|p| p.1),
        blocked: false,
        observed_at: now,
    })
}

/// The short text Claude Code shows as the status line:
/// `pq <model> 5h 75% · 7d 89%` (parts that are unknown are left out).
pub fn status_line_text(payload: &serde_json::Value, observed: Option<&ObservedUsage>) -> String {
    let mut out = String::from("pq");
    let model = payload.get("model");
    let name = model
        .and_then(|m| m.get("id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|id| ModelTier::from_model_id(id).to_string())
        .or_else(|| model.and_then(|m| m.get("display_name")).and_then(|v| v.as_str()).map(str::to_string));
    if let Some(name) = name {
        out.push(' ');
        out.push_str(&name);
    }
    let mut parts = Vec::new();
    if let Some(o) = observed {
        if let Some(w) = o.window_used {
            parts.push(format!("5h {:.0}%", w * 100.0));
        }
        if let Some(p) = o.period_used {
            parts.push(format!("7d {:.0}%", p * 100.0));
        }
    }
    if !parts.is_empty() {
        out.push(' ');
        out.push_str(&parts.join(" · "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::probe::save_observed;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    /// Abridged payload verified with Claude Code 2.1.287.
    fn payload() -> serde_json::Value {
        serde_json::json!({
            "session_id": "936b3978-0000-0000-0000-000000000000",
            "model": { "id": "claude-sonnet-5-5", "display_name": "Sonnet 5.5" },
            "version": "2.1.287",
            "cost": { "total_cost_usd": 0.0419 },
            "rate_limits": {
                "five_hour": { "used_percentage": 75, "resets_at": 1790914200 },
                "seven_day": { "used_percentage": 89, "resets_at": 1790920800 }
            }
        })
    }

    #[test]
    fn status_line_rate_limits_become_an_observation() {
        let obs = parse_status_line(&payload(), now()).unwrap();
        assert_eq!(obs.window_used, Some(0.75));
        assert_eq!(obs.window_resets_at, DateTime::from_timestamp(1790914200, 0));
        assert_eq!(obs.period_used, Some(0.89));
        assert_eq!(obs.period_resets_at, DateTime::from_timestamp(1790920800, 0));
        assert!(!obs.blocked);
        assert_eq!(obs.observed_at, now());
        assert_eq!(status_line_text(&payload(), Some(&obs)), "pq sonnet 5h 75% · 7d 89%");
    }

    #[test]
    fn missing_rate_limits_learn_nothing() {
        let mut p = payload();
        p.as_object_mut().unwrap().remove("rate_limits");
        assert!(parse_status_line(&p, now()).is_none());
        assert_eq!(status_line_text(&p, None), "pq sonnet");
        assert!(parse_status_line(&serde_json::json!({ "rate_limits": {} }), now()).is_none());
        assert_eq!(status_line_text(&serde_json::json!({}), None), "pq");
        let only_week = serde_json::json!({ "rate_limits": { "seven_day": { "used_percentage": 10 } } });
        let obs = parse_status_line(&only_week, now()).unwrap();
        assert_eq!((obs.window_used, obs.period_used, obs.period_resets_at), (None, Some(0.1), None));
        let named = serde_json::json!({ "model": { "display_name": "Mystery" } });
        assert_eq!(status_line_text(&named, Some(&obs)), "pq Mystery 7d 10%");
    }

    #[test]
    fn probe_returns_what_the_hook_stored() {
        let store = Store::open_in_memory().unwrap();
        let probe = ClaudeProbe::new(store.clone());
        assert_eq!(probe.provider(), Provider::Claude);
        assert!(probe.probe().unwrap().is_none());
        let obs = parse_status_line(&payload(), now()).unwrap();
        save_observed(&store, Provider::Claude, &obs).unwrap();
        assert_eq!(probe.probe().unwrap(), Some(obs));
    }
}
