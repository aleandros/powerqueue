//! Runs the real Codex usage probe against the installed `codex` binary.
//!
//! Skips itself unless `codex` is on PATH and `~/.codex/auth.json` (or
//! `$CODEX_HOME/auth.json`) exists. The app-server call consumes no quota.

use std::path::PathBuf;

use powerqueue::budget::UsageProbe;
use powerqueue::budget::probes::codex::CodexProbe;

fn codex_auth() -> Option<PathBuf> {
    let home = match std::env::var_os("CODEX_HOME") {
        Some(h) => PathBuf::from(h),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".codex"),
    };
    let auth = home.join("auth.json");
    auth.exists().then_some(auth)
}

#[test]
fn real_codex_app_server_reports_rate_limits() {
    let Ok(binary) = which::which("codex") else {
        eprintln!("skipping: codex is not on PATH");
        return;
    };
    if codex_auth().is_none() {
        eprintln!("skipping: no codex auth.json");
        return;
    }
    let probe = CodexProbe::new(&binary.to_string_lossy());
    let observed = probe.probe().expect("the codex app-server answers account/rateLimits/read");
    let observed = observed.expect("a logged-in account reports at least one bucket");
    let used = observed.period_used.or(observed.window_used).expect("a usage fraction");
    assert!((0.0..=1.5).contains(&used), "{observed:?}");
    if let Some(reset) = observed.period_resets_at.or(observed.window_resets_at) {
        assert!(reset > chrono::Utc::now() - chrono::Duration::days(1), "{observed:?}");
    }
}
