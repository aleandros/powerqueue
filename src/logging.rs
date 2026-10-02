//! Logging setup.
//!
//! * stderr: human-readable, colour when attached to a TTY, level from `-v`/`RUST_LOG`.
//! * file: `<state_dir>/logs/powerqueue.log.<date>` as JSON lines (default) or text,
//!   rotated daily, pruned to `logging.keep_days`.
//!
//! The returned guard must be kept alive for the process lifetime or buffered
//! lines are lost.

use std::path::Path;

use anyhow::Result;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::LoggingConfig;
use crate::paths::Paths;

/// How chatty stderr should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    Normal,
    Verbose,
    Trace,
}

impl Verbosity {
    pub fn from_flags(verbose: u8, quiet: bool) -> Self {
        if quiet {
            Verbosity::Quiet
        } else {
            match verbose {
                0 => Verbosity::Normal,
                1 => Verbosity::Verbose,
                _ => Verbosity::Trace,
            }
        }
    }
    fn stderr_filter(&self) -> &'static str {
        match self {
            Verbosity::Quiet => "error",
            Verbosity::Normal => "warn,powerqueue=info",
            Verbosity::Verbose => "info,powerqueue=debug",
            Verbosity::Trace => "debug,powerqueue=trace",
        }
    }
}

/// Initialise global tracing. `stderr` may be disabled for TUI commands.
pub fn init(paths: &Paths, cfg: &LoggingConfig, verbosity: Verbosity, stderr: bool) -> Result<WorkerGuard> {
    std::fs::create_dir_all(paths.logs_dir())?;
    prune_old_logs(&paths.logs_dir(), cfg.keep_days);

    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("powerqueue.log")
        .build(paths.logs_dir())?;
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let file_filter = EnvFilter::try_new(&cfg.level).unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer: Box<dyn Layer<_> + Send + Sync> = if cfg.json {
        Box::new(
            fmt::layer().json().with_current_span(false).with_span_list(false).with_writer(file_writer).with_filter(file_filter),
        )
    } else {
        Box::new(fmt::layer().with_ansi(false).with_writer(file_writer).with_filter(file_filter))
    };

    let stderr_filter = EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new(verbosity.stderr_filter()));
    let stderr_layer = if stderr {
        Some(
            fmt::layer()
                .with_target(verbosity == Verbosity::Trace)
                .without_time()
                .with_writer(std::io::stderr)
                .with_filter(stderr_filter),
        )
    } else {
        None
    };

    tracing_subscriber::registry().with(file_layer).with(stderr_layer).try_init().ok();
    Ok(guard)
}

/// Delete rotated log files older than `keep_days`.
pub fn prune_old_logs(dir: &Path, keep_days: u32) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(u64::from(keep_days) * 86_400);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("powerqueue.log") {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && modified < cutoff
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// All log files, newest first.
pub fn log_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("powerqueue.log")).unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files.reverse();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_mapping() {
        assert_eq!(Verbosity::from_flags(0, false), Verbosity::Normal);
        assert_eq!(Verbosity::from_flags(1, false), Verbosity::Verbose);
        assert_eq!(Verbosity::from_flags(3, false), Verbosity::Trace);
        assert_eq!(Verbosity::from_flags(3, true), Verbosity::Quiet);
    }

    #[test]
    fn prune_removes_old_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("powerqueue.log.2020-01-01");
        let fresh = dir.path().join("powerqueue.log.2099-01-01");
        let other = dir.path().join("keep.txt");
        for f in [&old, &fresh, &other] {
            std::fs::write(f, "x").unwrap();
        }
        let ancient = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        let file = std::fs::File::open(&old).unwrap();
        file.set_modified(ancient).unwrap();
        prune_old_logs(dir.path(), 7);
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(other.exists());
        assert_eq!(log_files(dir.path()), vec![fresh]);
    }
}
