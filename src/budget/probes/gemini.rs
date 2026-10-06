//! Antigravity (`agy`) usage probe: `agy -p "/usage" --output-format json
//! --print-timeout 20s`. Experimental: the output shape comes from community
//! reports and could not be verified here, so anything unexpected is
//! `Ok(None)` with a debug log rather than an error that stops the daemon.
//!
//! Expected: `command.data.groups[].buckets[]` with ids `gemini-5h` (the
//! rolling window) and `gemini-weekly` (the period), each carrying
//! `remaining_fraction` (0–1) and `disabled` (the quota is exhausted).

use std::process::Command;
use std::time::Duration as StdDuration;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};

use crate::budget::probe::{ObservedUsage, UsageProbe};
use crate::domain::Provider;

use super::{PROBE_TIMEOUT, epoch_secs, first_line, run_with_timeout};

/// Bucket id of the 5-hour window.
pub const WINDOW_BUCKET: &str = "gemini-5h";
/// Bucket id of the weekly cap.
pub const PERIOD_BUCKET: &str = "gemini-weekly";

/// Runs `agy`'s headless `/usage` command.
#[derive(Debug, Clone)]
pub struct GeminiProbe {
    pub binary: String,
    pub timeout: StdDuration,
}

impl GeminiProbe {
    pub fn new(binary: &str) -> Self {
        Self { binary: binary.to_string(), timeout: PROBE_TIMEOUT }
    }

    /// The argument list after the binary.
    pub fn args() -> [&'static str; 6] {
        ["-p", "/usage", "--output-format", "json", "--print-timeout", "20s"]
    }
}

impl UsageProbe for GeminiProbe {
    fn provider(&self) -> Provider {
        Provider::Gemini
    }

    /// Fails when the binary cannot start, exits non-zero or exceeds the
    /// timeout; `Ok(None)` for output it does not understand.
    fn probe(&self) -> Result<Option<ObservedUsage>> {
        let out = run_with_timeout(Command::new(&self.binary).args(Self::args()), self.timeout)?;
        if !out.status.success() {
            let detail = match first_line(&out.stderr) {
                s if s.is_empty() => first_line(&out.stdout),
                s => s,
            };
            bail!("`{} -p /usage` exited with {}: {detail}", self.binary, out.status);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let Some(value) = json_in(&text) else {
            tracing::debug!(output = %first_line(&out.stdout), "agy /usage: output is not JSON");
            return Ok(None);
        };
        let parsed = parse_usage(&value, Utc::now());
        if parsed.is_none() {
            tracing::debug!(%value, "agy /usage: unrecognised shape");
        }
        Ok(parsed)
    }
}

/// The JSON document in `text`: the whole text, else the span from the first
/// `{` to the last `}` (CLIs sometimes print a banner around it).
fn json_in(text: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str(text.trim()) {
        return Some(v);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| serde_json::from_str(&text[start..=end]).ok()).flatten()
}

/// Decode `/usage` JSON into an [`ObservedUsage`] taken at `now`. Used
/// fraction = 1 − `remaining_fraction`; a `disabled` bucket sets `blocked`;
/// a reset instant is read from `reset_time` / `resets_at` / `resetTime`
/// (RFC 3339 or unix seconds) when present. `None` when neither known bucket
/// is there.
pub fn parse_usage(value: &serde_json::Value, now: DateTime<Utc>) -> Option<ObservedUsage> {
    let groups = value.pointer("/command/data/groups").and_then(|g| g.as_array())?;
    let mut obs = ObservedUsage::empty(now);
    let mut learned = false;
    for bucket in groups.iter().filter_map(|g| g.get("buckets").and_then(|b| b.as_array())).flatten() {
        let Some(id) = bucket.get("id").and_then(|v| v.as_str()) else { continue };
        let is_window = match id {
            WINDOW_BUCKET => true,
            PERIOD_BUCKET => false,
            _ => continue,
        };
        let remaining = bucket.get("remaining_fraction").or_else(|| bucket.get("remainingFraction")).and_then(|v| v.as_f64());
        let used = remaining.map(|r| (1.0 - r).clamp(0.0, 1.0));
        let disabled = bucket.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false);
        let resets = ["reset_time", "resets_at", "resetTime", "reset_at"].iter().find_map(|k| {
            let v = bucket.get(*k)?;
            epoch_secs(v).or_else(|| v.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc)))
        });
        if used.is_none() && !disabled {
            continue;
        }
        if is_window {
            obs.window_used = used.or(Some(1.0));
            obs.window_resets_at = resets;
        } else {
            obs.period_used = used.or(Some(1.0));
            obs.period_resets_at = resets;
        }
        obs.blocked |= disabled;
        learned = true;
    }
    learned.then_some(obs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    /// Hand-written from the community description of `agy -p /usage --output-format json`.
    fn sample() -> serde_json::Value {
        serde_json::json!({
            "num_turns": 0,
            "command": {
                "name": "usage",
                "data": {
                    "groups": [
                        {
                            "name": "Gemini Models",
                            "buckets": [
                                { "id": "gemini-5h", "remaining_fraction": 0.6, "disabled": false, "window_minutes": 300,
                                  "reset_time": "2026-10-01T14:00:00Z" },
                                { "id": "gemini-weekly", "remaining_fraction": 0.25, "disabled": false, "window_minutes": 10080 }
                            ]
                        },
                        {
                            "name": "Third-party Models",
                            "buckets": [
                                { "id": "3p-5h", "remaining_fraction": 0.0, "disabled": true },
                                { "id": "3p-weekly", "remaining_fraction": 1.0, "disabled": false }
                            ]
                        }
                    ]
                }
            }
        })
    }

    #[test]
    fn parses_window_and_weekly_buckets() {
        let obs = parse_usage(&sample(), now()).unwrap();
        assert!((obs.window_used.unwrap() - 0.4).abs() < 1e-9);
        assert_eq!(obs.window_resets_at, Some(DateTime::parse_from_rfc3339("2026-10-01T14:00:00Z").unwrap().with_timezone(&Utc)));
        assert!((obs.period_used.unwrap() - 0.75).abs() < 1e-9);
        assert_eq!(obs.period_resets_at, None);
        assert!(!obs.blocked, "third-party buckets do not count");
        assert_eq!(obs.observed_at, now());
    }

    #[test]
    fn disabled_bucket_blocks() {
        let mut v = sample();
        v["command"]["data"]["groups"][0]["buckets"][0] =
            serde_json::json!({ "id": "gemini-5h", "disabled": true, "reset_time": 1790914200 });
        let obs = parse_usage(&v, now()).unwrap();
        assert!(obs.blocked);
        assert_eq!(obs.window_used, Some(1.0));
        assert_eq!(obs.window_resets_at, DateTime::from_timestamp(1790914200, 0));
        assert_eq!(obs.cooldown_until(), DateTime::from_timestamp(1790914200, 0));
    }

    #[test]
    fn unexpected_shapes_learn_nothing() {
        assert!(parse_usage(&serde_json::json!({}), now()).is_none());
        assert!(parse_usage(&serde_json::json!({ "command": { "data": { "groups": [] } } }), now()).is_none());
        let other = serde_json::json!({ "command": { "data": { "groups": [{ "buckets": [{ "id": "3p-5h", "remaining_fraction": 0.5 }] }] } } });
        assert!(parse_usage(&other, now()).is_none());
        assert_eq!(json_in("banner\n{\"a\":1}\ntrailer"), Some(serde_json::json!({ "a": 1 })));
        assert_eq!(json_in("nothing here"), None);
    }

    #[cfg(unix)]
    #[test]
    fn probe_runs_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            let path = dir.path().join(name);
            crate::test_support::write_executable(&path, body);
            path.display().to_string()
        };
        let ok = write(
            "agy-ok",
            &format!(
                "[ \"$*\" = \"-p /usage --output-format json --print-timeout 20s\" ] || exit 9\ncat <<'EOF'\n{}\nEOF",
                sample()
            ),
        );
        let obs = GeminiProbe::new(&ok).probe().unwrap().unwrap();
        assert!((obs.period_used.unwrap() - 0.75).abs() < 1e-9);
        let junk = write("agy-junk", "echo 'Please sign in'");
        assert!(GeminiProbe::new(&junk).probe().unwrap().is_none());
        let fails = write("agy-fails", "echo 'not signed in' >&2; exit 1");
        let err = GeminiProbe::new(&fails).probe().unwrap_err().to_string();
        assert!(err.contains("not signed in"), "{err}");
        assert!(GeminiProbe::new("/definitely/not/agy").probe().is_err());
        assert_eq!(GeminiProbe::new("agy").provider(), Provider::Gemini);
    }
}
