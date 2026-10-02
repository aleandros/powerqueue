//! The real usage probes, one per provider, and the code that runs them.
//!
//! * [`codex`]  – `codex app-server` → `account/rateLimits/read` (and the
//!   same parser for the rollout `token_count.rate_limits` snapshot).
//! * [`claude`] – reads what the status-line hook stored; no network call.
//! * [`gemini`] – `agy -p "/usage" --output-format json` (experimental).
//!
//! `crate::session::agent::AgentCli::probe` delegates here, so every caller
//! goes through the provider abstraction. [`probe_all`] runs the probes of
//! the enabled providers, stores what they learn (kv
//! `budget.observed.<provider>`), remembers the outcome (kv
//! `budget.probe_status.<provider>`, for `doctor` and `budget show`) and logs
//! `budget.probe` events: info when the observation changes, warn on failure
//! (at most once per provider per hour).
//!
//! A probe never blocks for more than [`PROBE_TIMEOUT`]; callers on the
//! daemon's async runtime run [`probe_all`] on a blocking thread.

pub mod claude;
pub mod codex;
pub mod gemini;

use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::domain::{EventLevel, Provider};
use crate::store::Store;

use super::probe::{ObservedUsage, load_observed, save_observed};

/// Longest a single probe may take, including process start-up.
pub const PROBE_TIMEOUT: StdDuration = StdDuration::from_secs(15);

/// A failing probe is reported as a `warn` event at most this often.
pub const PROBE_WARN_EVERY: Duration = Duration::hours(1);

/// kv key holding the outcome of a provider's latest probe.
pub fn probe_status_key(provider: Provider) -> String {
    format!("budget.probe_status.{provider}")
}

/// kv key holding when a provider's failing probe last produced a warning.
fn probe_warned_key(provider: Provider) -> String {
    format!("budget.probe_warned.{provider}")
}

/// Outcome of a provider's latest probe run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeStatus {
    pub at: DateTime<Utc>,
    /// `true` when the probe ran without error (it may still have learned nothing).
    pub ok: bool,
    /// `true` when it returned an observation.
    pub learned: bool,
    pub error: Option<String>,
}

/// The latest probe outcome of a provider, if a probe ever ran. Fails only
/// when the database cannot be read.
pub fn load_probe_status(store: &Store, provider: Provider) -> Result<Option<ProbeStatus>> {
    store.kv_get(&probe_status_key(provider)).with_context(|| format!("read probe status of {provider}"))
}

/// Run the probe of every enabled provider (in `budget.provider_order`) and
/// record the results; see [`probe_providers`].
pub fn probe_all(cfg: &Config, store: &Store, now: DateTime<Utc>) -> Vec<(Provider, Result<Option<ObservedUsage>>)> {
    probe_providers(cfg, store, &cfg.budget.enabled_providers_in_order(), now)
}

/// Run the probes of `providers` (enabled or not) through
/// `agent_for(p).probe` and record each result: an observation is saved
/// with [`save_observed`] and logged as an info `budget.probe` event when it
/// differs from the stored one; a failure is logged as a warn `budget.probe`
/// event at most once per [`PROBE_WARN_EVERY`]; the outcome lands in kv
/// [`probe_status_key`]. Store errors while recording are traced, never
/// returned: the result list always has one entry per provider.
pub fn probe_providers(
    cfg: &Config,
    store: &Store,
    providers: &[Provider],
    now: DateTime<Utc>,
) -> Vec<(Provider, Result<Option<ObservedUsage>>)> {
    providers
        .iter()
        .map(|p| {
            let result = crate::session::agent_for(*p).probe(cfg, store);
            if let Err(e) = record(store, *p, &result, now) {
                tracing::warn!(provider = %p, error = %format!("{e:#}"), "cannot record probe result");
            }
            (*p, result)
        })
        .collect()
}

/// Persist and log one probe result.
fn record(store: &Store, provider: Provider, result: &Result<Option<ObservedUsage>>, now: DateTime<Utc>) -> Result<()> {
    let status = match result {
        Ok(obs) => ProbeStatus { at: now, ok: true, learned: obs.is_some(), error: None },
        Err(e) => ProbeStatus { at: now, ok: false, learned: false, error: Some(format!("{e:#}")) },
    };
    store.kv_set(&probe_status_key(provider), &status)?;
    match result {
        Ok(Some(obs)) => {
            let previous = load_observed(store, provider).ok().flatten();
            if previous.as_ref() == Some(obs) {
                return Ok(());
            }
            save_observed(store, provider, obs)?;
            let changed = previous.as_ref().is_none_or(|p| !same_reading(p, obs));
            if changed {
                store.log_event(
                    None,
                    None,
                    EventLevel::Info,
                    "budget.probe",
                    &format!("{provider} usage: {}", describe(obs)),
                    serde_json::json!({ "provider": provider, "observed": obs }),
                )?;
            }
            tracing::debug!(provider = %provider, changed, "probe stored an observation");
        }
        Ok(None) => tracing::debug!(provider = %provider, "probe learned nothing"),
        Err(e) => {
            let last: Option<DateTime<Utc>> = store.kv_get(&probe_warned_key(provider))?;
            tracing::debug!(provider = %provider, error = %format!("{e:#}"), "probe failed");
            if last.is_none_or(|t| now - t >= PROBE_WARN_EVERY) {
                store.kv_set(&probe_warned_key(provider), &now)?;
                store.log_event(
                    None,
                    None,
                    EventLevel::Warn,
                    "budget.probe",
                    &format!("{provider} usage probe failed: {e:#}"),
                    serde_json::json!({ "provider": provider, "error": format!("{e:#}") }),
                )?;
            }
        }
    }
    Ok(())
}

/// Two observations say the same thing (ignoring when they were taken).
fn same_reading(a: &ObservedUsage, b: &ObservedUsage) -> bool {
    let pct = |v: Option<f64>| v.map(|f| (f * 1000.0).round() as i64);
    pct(a.window_used) == pct(b.window_used)
        && pct(a.period_used) == pct(b.period_used)
        && a.window_resets_at == b.window_resets_at
        && a.period_resets_at == b.period_resets_at
        && a.blocked == b.blocked
}

/// One-line human summary: `window 17% (resets 14:30 UTC) · period 40% (resets 2026-10-05 07:00 UTC) · blocked`.
pub fn describe(obs: &ObservedUsage) -> String {
    let mut parts = Vec::new();
    if let Some(u) = obs.window_used {
        let reset = obs.window_resets_at.map(|r| format!(" (resets {})", r.format("%Y-%m-%d %H:%M UTC"))).unwrap_or_default();
        parts.push(format!("window {:.0}%{reset}", u * 100.0));
    }
    if let Some(u) = obs.period_used {
        let reset = obs.period_resets_at.map(|r| format!(" (resets {})", r.format("%Y-%m-%d %H:%M UTC"))).unwrap_or_default();
        parts.push(format!("period {:.0}%{reset}", u * 100.0));
    }
    if obs.blocked {
        parts.push("blocked".to_string());
    }
    if parts.is_empty() { "nothing reported".to_string() } else { parts.join(" · ") }
}

/// Convert unix epoch seconds (integer or float JSON number) to UTC.
pub(crate) fn epoch_secs(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    let secs = v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64))?;
    DateTime::from_timestamp(secs, 0)
}

/// Run `cmd` with its output captured, killing it after `timeout`. Fails
/// when it cannot be started (with the binary named), when it times out, or
/// when reading its output fails; a non-zero exit is returned in the
/// [`Output`] for the caller to judge.
pub(crate) fn run_with_timeout(cmd: &mut Command, timeout: StdDuration) -> Result<Output> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run `{program}`"))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let status = match wait_until(&mut child, Instant::now() + timeout)? {
        Some(status) => status,
        None => {
            kill(&mut child);
            bail!("`{program}` did not finish within {}s", timeout.as_secs());
        }
    };
    let stdout = collect(&stdout, PIPE_GRACE);
    let stderr = collect(&stderr, PIPE_GRACE);
    Ok(Output { status, stdout, stderr })
}

/// How long to wait for a pipe's end after its process exited (a grandchild
/// may keep it open; we do not wait for that).
pub(crate) const PIPE_GRACE: StdDuration = StdDuration::from_millis(1000);

/// Read a pipe to the end on its own thread; the contents arrive on the
/// returned channel at EOF.
pub(crate) fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

/// The contents of a [`drain`]ed pipe, or nothing when EOF does not come within `wait`.
pub(crate) fn collect(rx: &std::sync::mpsc::Receiver<Vec<u8>>, wait: StdDuration) -> Vec<u8> {
    rx.recv_timeout(wait).unwrap_or_default()
}

/// Poll `child` until it exits or `deadline` passes (`Ok(None)`).
pub(crate) fn wait_until(child: &mut Child, deadline: Instant) -> Result<Option<std::process::ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait().context("wait for probe process")? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(StdDuration::from_millis(25));
    }
}

/// Kill and reap a child, ignoring errors (it may already be gone).
pub(crate) fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// First non-empty line of a byte buffer, for error messages.
pub(crate) fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn describe_and_epoch() {
        let mut obs = ObservedUsage::empty(at("2026-10-01T12:00:00Z"));
        assert_eq!(describe(&obs), "nothing reported");
        obs.window_used = Some(0.17);
        obs.window_resets_at = Some(at("2026-10-01T14:30:00Z"));
        obs.period_used = Some(0.4);
        obs.blocked = true;
        assert_eq!(describe(&obs), "window 17% (resets 2026-10-01 14:30 UTC) · period 40% · blocked");
        assert_eq!(epoch_secs(&serde_json::json!(1791234430)), Some(at("2026-10-05T21:07:10Z")));
        assert_eq!(epoch_secs(&serde_json::json!(1791234430.4)), Some(at("2026-10-05T21:07:10Z")));
        assert_eq!(epoch_secs(&serde_json::json!("x")), None);
    }

    #[test]
    fn record_saves_logs_changes_and_rate_limits_warnings() {
        let store = Store::open_in_memory().unwrap();
        let now = at("2026-10-01T12:00:00Z");
        let obs = ObservedUsage { period_used: Some(0.3), ..ObservedUsage::empty(now) };
        record(&store, Provider::Codex, &Ok(Some(obs.clone())), now).unwrap();
        assert_eq!(load_observed(&store, Provider::Codex).unwrap(), Some(obs.clone()));
        let status = load_probe_status(&store, Provider::Codex).unwrap().unwrap();
        assert!(status.ok && status.learned);
        // Same reading later: stored (fresh timestamp) but no new event.
        let later = ObservedUsage { observed_at: now + Duration::minutes(15), ..obs.clone() };
        record(&store, Provider::Codex, &Ok(Some(later.clone())), now + Duration::minutes(15)).unwrap();
        assert_eq!(load_observed(&store, Provider::Codex).unwrap(), Some(later));
        let events = store.recent_events(50).unwrap();
        assert_eq!(events.iter().filter(|e| e.kind == "budget.probe").count(), 1);
        assert!(events.iter().any(|e| e.message == "codex usage: period 30%"), "{events:?}");

        // Failures warn once per hour.
        for mins in [0, 10, 59, 61] {
            let t = now + Duration::minutes(mins);
            record(&store, Provider::Gemini, &Err(anyhow::anyhow!("agy: not found")), t).unwrap();
        }
        let events = store.recent_events(50).unwrap();
        let warns: Vec<_> = events.iter().filter(|e| e.kind == "budget.probe" && e.level == EventLevel::Warn).collect();
        assert_eq!(warns.len(), 2, "{warns:?}");
        let status = load_probe_status(&store, Provider::Gemini).unwrap().unwrap();
        assert!(!status.ok);
        assert_eq!(status.error.as_deref(), Some("agy: not found"));
        record(&store, Provider::Gemini, &Ok(None), now).unwrap();
        assert!(load_probe_status(&store, Provider::Gemini).unwrap().unwrap().ok);
    }

    #[test]
    fn probe_all_covers_enabled_providers() {
        let store = Store::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.codex.binary = "/definitely/not/codex".into();
        let now = Utc::now();
        let results = probe_all(&cfg, &store, now);
        assert_eq!(results.len(), 1, "only claude is enabled by default");
        assert_eq!(results[0].0, Provider::Claude);
        assert!(results[0].1.as_ref().unwrap().is_none(), "no status line seen yet");
        cfg.budget.providers.codex.enabled = true;
        cfg.budget.provider_order = vec![Provider::Codex];
        let results = probe_all(&cfg, &store, now);
        assert_eq!(results.iter().map(|(p, _)| *p).collect::<Vec<_>>(), vec![Provider::Codex, Provider::Claude]);
        let err = results[0].1.as_ref().unwrap_err().to_string();
        assert!(err.contains("/definitely/not/codex"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_slow_commands() {
        if which::which("sleep").is_err() {
            return;
        }
        let started = Instant::now();
        let err = run_with_timeout(Command::new("sleep").arg("5"), StdDuration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err}");
        assert!(started.elapsed() < StdDuration::from_secs(3));
        let out = run_with_timeout(Command::new("sh").args(["-c", "echo hi; echo oops >&2; exit 3"]), PROBE_TIMEOUT).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(first_line(&out.stdout), "hi");
        assert_eq!(first_line(&out.stderr), "oops");
    }
}
