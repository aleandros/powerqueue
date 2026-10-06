//! Codex usage probe: `codex app-server` → `account/rateLimits/read`.
//!
//! The app-server speaks JSON-RPC 2.0 over stdio, one message per line. We
//! send `initialize` (id 0, with `capabilities.experimentalApi`),
//! `initialized` and `account/rateLimits/read` (id 1), read stdout until the
//! response with id 1 (notifications without an id are ignored), then kill
//! the process. The call is fast and consumes no quota.
//!
//! [`parse_rate_limits`] also decodes the snake_case `rate_limits` snapshot
//! Codex writes into rollout `token_count` events, so the transcript tailer
//! can feed the same [`ObservedUsage`].

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};

use crate::budget::probe::{ObservedUsage, UsageProbe};
use crate::domain::Provider;

use super::{PIPE_GRACE, PROBE_TIMEOUT, collect, drain, epoch_secs, first_line, kill};

/// Buckets at most this long (minutes) are the rolling window; longer ones
/// are the period (Codex reports 300 for the 5-hour window, 10080 weekly).
pub const WINDOW_MAX_MINUTES: i64 = 600;

/// Asks a Codex CLI binary for its account's rate limits.
#[derive(Debug, Clone)]
pub struct CodexProbe {
    pub binary: String,
    pub timeout: StdDuration,
}

impl CodexProbe {
    pub fn new(binary: &str) -> Self {
        Self { binary: binary.to_string(), timeout: PROBE_TIMEOUT }
    }

    /// The three JSON-RPC lines the probe writes.
    pub fn requests() -> [serde_json::Value; 3] {
        [
            serde_json::json!({
                "method": "initialize",
                "id": 0,
                "params": {
                    "clientInfo": { "name": "powerqueue", "title": "powerqueue", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "experimentalApi": true }
                }
            }),
            serde_json::json!({ "method": "initialized", "params": {} }),
            serde_json::json!({ "method": "account/rateLimits/read", "id": 1, "params": { "excludeResetCreditDetails": true } }),
        ]
    }
}

impl UsageProbe for CodexProbe {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    /// Spawn `<binary> -s read-only -a never app-server`, ask for the rate
    /// limits and parse them. Fails when the binary cannot start, exits or
    /// answers with an error before replying, or takes longer than
    /// `timeout`; `Ok(None)` when the reply carries no usable bucket.
    fn probe(&self) -> Result<Option<ObservedUsage>> {
        let mut child = Command::new(&self.binary)
            .args(["-s", "read-only", "-a", "never", "app-server"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("run `{} app-server`", self.binary))?;
        let stderr = drain(child.stderr.take());
        let result = self.converse(&mut child);
        kill(&mut child);
        let stderr = if result.is_err() { collect(&stderr, PIPE_GRACE) } else { Vec::new() };
        result.map_err(|e| {
            let detail = first_line(&stderr);
            if detail.is_empty() { e } else { e.context(format!("stderr: {detail}")) }
        })
    }
}

impl CodexProbe {
    fn converse(&self, child: &mut std::process::Child) -> Result<Option<ObservedUsage>> {
        let deadline = Instant::now() + self.timeout;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("app-server stdout is not piped"))?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        {
            let stdin = child.stdin.as_mut().ok_or_else(|| anyhow!("app-server stdin is not piped"))?;
            for req in Self::requests() {
                writeln!(stdin, "{req}").context("write to codex app-server")?;
            }
            stdin.flush().context("write to codex app-server")?;
        }
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("codex app-server did not answer within {}s", self.timeout.as_secs());
            }
            let line = match rx.recv_timeout(remaining) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!("codex app-server did not answer within {}s", self.timeout.as_secs())
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("codex app-server exited before answering"),
            };
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                tracing::debug!(line = %line, "codex app-server: ignoring a non-JSON line");
                continue;
            };
            let Some(id) = msg.get("id").and_then(|v| v.as_i64()) else { continue };
            if let Some(err) = msg.get("error") {
                let text = err.get("message").and_then(|m| m.as_str()).map(str::to_string).unwrap_or_else(|| err.to_string());
                bail!("codex app-server request {id} failed: {text}");
            }
            if id == 1 {
                let result = msg.get("result").cloned().unwrap_or(serde_json::Value::Null);
                let parsed = parse_rate_limits(&result);
                if parsed.is_none() {
                    tracing::debug!(%result, "codex rate limits: unrecognised shape");
                }
                return Ok(parsed);
            }
        }
    }
}

/// Decode Codex rate limits into an [`ObservedUsage`] taken now. See
/// [`parse_rate_limits_at`].
pub fn parse_rate_limits(value: &serde_json::Value) -> Option<ObservedUsage> {
    parse_rate_limits_at(value, Utc::now())
}

/// Decode Codex rate limits, accepting:
///
/// * the `account/rateLimits/read` result (`{"ordinaryUsageAllowed": …,
///   "rateLimits": {"primary": {"usedPercent", "windowDurationMins",
///   "resetsAt"}, "secondary": …}}`),
/// * a bare `rateLimits` object (camelCase), or
/// * the rollout snapshot (`{"primary": {"used_percent", "window_minutes",
///   "resets_at"}, …}`, also when wrapped as `{"rate_limits": …}`).
///
/// Each bucket goes to the window when its duration is at most
/// [`WINDOW_MAX_MINUTES`], else to the period (a bucket without a duration:
/// `primary` = window, `secondary` = period). Percentages become fractions;
/// `resetsAt` is unix seconds. `ordinaryUsageAllowed == false` or a non-null
/// `rateLimitReachedType` sets `blocked`. `None` when nothing usable is there.
pub fn parse_rate_limits_at(value: &serde_json::Value, now: DateTime<Utc>) -> Option<ObservedUsage> {
    let field = |v: &serde_json::Value, names: &[&str]| names.iter().find_map(|n| v.get(*n).filter(|x| !x.is_null()).cloned());
    let (limits, outer) = match field(value, &["rateLimits", "rate_limits"]) {
        Some(inner) => (inner, Some(value)),
        None => (value.clone(), None),
    };
    if !limits.is_object() {
        return None;
    }
    let mut obs = ObservedUsage::empty(now);
    let mut learned = false;
    for (slot, key) in ["primary", "secondary"].into_iter().enumerate() {
        let Some(bucket) = field(&limits, &[key]) else { continue };
        let Some(used) = field(&bucket, &["usedPercent", "used_percent"]).and_then(|v| v.as_f64()) else { continue };
        let minutes = field(&bucket, &["windowDurationMins", "window_minutes", "window_duration_mins"]).and_then(|v| v.as_i64());
        let resets = field(&bucket, &["resetsAt", "resets_at"]).and_then(|v| epoch_secs(&v));
        let is_window = match minutes {
            Some(m) => m <= WINDOW_MAX_MINUTES,
            None => slot == 0,
        };
        let used = (used / 100.0).max(0.0);
        if is_window {
            obs.window_used = Some(used);
            obs.window_resets_at = resets;
        } else {
            obs.period_used = Some(used);
            obs.period_resets_at = resets;
        }
        learned = true;
    }
    let allowed = outer.and_then(|o| field(o, &["ordinaryUsageAllowed", "ordinary_usage_allowed"])).and_then(|v| v.as_bool());
    let reached = field(&limits, &["rateLimitReachedType", "rate_limit_reached_type"]).is_some();
    if allowed == Some(false) || reached {
        obs.blocked = true;
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

    fn epoch(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    /// The `account/rateLimits/read` response verified on codex-cli 0.159.2 (prolite plan).
    const PRO_RESPONSE: &str = r#"{"id":1,"result":{"ordinaryUsageAllowed":true,"rateLimits":{"limitId":"codex","limitName":null,"normalModelSlug":null,
        "primary":{"usedPercent":17,"windowDurationMins":10080,"resetsAt":1791234430},"secondary":null,
        "credits":{"hasCredits":false,"unlimited":false,"balance":"0"},"individualLimit":null,"spendControlReached":false,
        "planType":"prolite","rateLimitReachedType":null},
        "rateLimitsByLimitId":{"codex":{}},"rateLimitResetCredits":{"availableCount":2,"credits":null},"accountId":"acc","rateLimitUpsell":null}}"#;

    #[test]
    fn pro_style_single_weekly_bucket_is_the_period() {
        let msg: serde_json::Value = serde_json::from_str(PRO_RESPONSE).unwrap();
        let obs = parse_rate_limits_at(&msg["result"], now()).unwrap();
        assert_eq!(obs.window_used, None, "Pro plans have no 5h window");
        assert_eq!(obs.period_used, Some(0.17));
        assert_eq!(obs.period_resets_at, Some(epoch(1791234430)));
        assert!(!obs.blocked);
        assert_eq!(obs.observed_at, now());
    }

    #[test]
    fn plus_style_two_buckets_split_window_and_period() {
        let result = serde_json::json!({
            "ordinaryUsageAllowed": true,
            "rateLimits": {
                "primary": { "usedPercent": 42.5, "windowDurationMins": 300, "resetsAt": 1790914200 },
                "secondary": { "usedPercent": 61, "windowDurationMins": 10080, "resetsAt": 1791234430 },
                "rateLimitReachedType": null
            }
        });
        let obs = parse_rate_limits_at(&result, now()).unwrap();
        assert_eq!(obs.window_used, Some(0.425));
        assert_eq!(obs.window_resets_at, Some(epoch(1790914200)));
        assert_eq!(obs.period_used, Some(0.61));
        assert_eq!(obs.period_resets_at, Some(epoch(1791234430)));
        assert!(!obs.blocked);
        assert_eq!(obs.cooldown_until(), None);
    }

    #[test]
    fn ordinary_usage_not_allowed_is_blocked() {
        let result = serde_json::json!({
            "ordinaryUsageAllowed": false,
            "rateLimits": {
                "primary": { "usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1790914200 },
                "secondary": { "usedPercent": 80, "windowDurationMins": 10080, "resetsAt": null },
                "rateLimitReachedType": "primary"
            }
        });
        let obs = parse_rate_limits_at(&result, now()).unwrap();
        assert!(obs.blocked);
        assert_eq!(obs.window_used, Some(1.0));
        assert_eq!(obs.period_resets_at, None);
        assert_eq!(obs.cooldown_until(), Some(epoch(1790914200)));
        // Blocked with no buckets at all still says something.
        let bare = serde_json::json!({ "ordinaryUsageAllowed": false, "rateLimits": { "primary": null } });
        assert!(parse_rate_limits_at(&bare, now()).unwrap().blocked);
    }

    #[test]
    fn rollout_snapshot_in_snake_case() {
        // `token_count.rate_limits` from a rollout written by codex-cli 0.159.2.
        let snapshot: serde_json::Value = serde_json::from_str(
            r#"{"limit_id":"codex","primary":{"used_percent":17.0,"window_minutes":10080,"resets_at":1791234430},"secondary":null,"credits":{"has_credits":false},"plan_type":"prolite","rate_limit_reached_type":null}"#,
        )
        .unwrap();
        let obs = parse_rate_limits_at(&snapshot, now()).unwrap();
        assert_eq!(obs.period_used, Some(0.17));
        assert_eq!(obs.period_resets_at, Some(epoch(1791234430)));
        assert_eq!(obs.window_used, None);
        assert!(!obs.blocked);
        // Wrapped in its `token_count` payload.
        let wrapped = serde_json::json!({ "rate_limits": snapshot });
        assert_eq!(parse_rate_limits_at(&wrapped, now()), Some(obs));
        // The public entry point stamps the current time.
        assert!(parse_rate_limits(&wrapped).is_some());
    }

    #[test]
    fn unknown_shapes_learn_nothing() {
        assert!(parse_rate_limits_at(&serde_json::json!(null), now()).is_none());
        assert!(parse_rate_limits_at(&serde_json::json!({ "rateLimits": null }), now()).is_none());
        assert!(parse_rate_limits_at(&serde_json::json!({ "primary": { "foo": 1 } }), now()).is_none());
        assert!(parse_rate_limits_at(&serde_json::json!([1, 2]), now()).is_none());
        // A bucket without a duration: primary is the window.
        let obs = parse_rate_limits_at(&serde_json::json!({ "primary": { "used_percent": 5 } }), now()).unwrap();
        assert_eq!(obs.window_used, Some(0.05));
    }

    #[test]
    fn requests_match_the_protocol() {
        let [init, initialized, read] = CodexProbe::requests();
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["id"], 0);
        assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);
        assert!(initialized.get("id").is_none());
        assert_eq!(read["method"], "account/rateLimits/read");
        assert_eq!(read["id"], 1);
        assert_eq!(read["params"]["excludeResetCreditDetails"], true);
    }

    #[cfg(unix)]
    fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        crate::test_support::write_executable(&path, body);
        path.display().to_string()
    }

    #[cfg(unix)]
    #[test]
    fn probe_talks_to_a_fake_app_server() {
        let dir = tempfile::tempdir().unwrap();
        // Reads the three requests, emits a notification first, then the answer.
        let ok = script(
            dir.path(),
            "codex-ok",
            r#"[ "$1 $2 $3 $4 $5" = "-s read-only -a never app-server" ] || { echo "bad args: $*" >&2; exit 2; }
read a; read b; read c
case "$c" in *account/rateLimits/read*) ;; *) echo "bad request" >&2; exit 3 ;; esac
echo '{"method":"remoteControl/status/changed","params":{}}'
echo 'not json'
echo '{"id":0,"result":{}}'
echo '{"id":1,"result":{"ordinaryUsageAllowed":true,"rateLimits":{"primary":{"usedPercent":17,"windowDurationMins":10080,"resetsAt":1791234430},"secondary":null}}}'
sleep 30"#,
        );
        let started = Instant::now();
        let obs = CodexProbe::new(&ok).probe().unwrap().unwrap();
        assert_eq!(obs.period_used, Some(0.17));
        assert!(started.elapsed() < StdDuration::from_secs(10), "the child is killed once answered");

        let err_srv =
            script(dir.path(), "codex-err", r#"read a; echo '{"id":1,"error":{"code":-32600,"message":"not logged in"}}'"#);
        let err = CodexProbe::new(&err_srv).probe().unwrap_err();
        assert!(format!("{err:#}").contains("not logged in"), "{err:#}");

        let dies = script(dir.path(), "codex-dies", "echo 'auth.json missing' >&2; exit 1");
        let err = CodexProbe::new(&dies).probe().unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("exited before answering") || text.contains("write to codex"), "{text}");

        let slow = script(dir.path(), "codex-slow", "sleep 30");
        let probe = CodexProbe { binary: slow, timeout: StdDuration::from_millis(300) };
        let started = Instant::now();
        let err = probe.probe().unwrap_err();
        assert!(err.to_string().contains("did not answer within"), "{err:#}");
        assert!(started.elapsed() < StdDuration::from_secs(5));

        assert!(CodexProbe::new("/definitely/not/codex").probe().is_err());
        assert_eq!(CodexProbe::new("codex").provider(), Provider::Codex);
    }
}
