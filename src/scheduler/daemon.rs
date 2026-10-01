//! Daemon state and main loop.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::config::Config;
use crate::paths::Paths;
use crate::secrets::Secrets;
use crate::store::Store;

/// Shared stop flag.
#[derive(Debug, Clone, Default)]
pub struct DaemonHandle {
    stop: Arc<AtomicBool>,
}

impl DaemonHandle {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// The long-running scheduler.
pub struct Daemon {
    pub cfg: Config,
    pub paths: Paths,
    pub store: Store,
    pub secrets: Secrets,
    pub handle: DaemonHandle,
    pub started_at: DateTime<Utc>,
    /// Exclusive lock file so only one daemon runs per data dir.
    pub lock_path: PathBuf,
}

impl Daemon {
    pub fn new(cfg: Config, paths: Paths, store: Store, secrets: Secrets) -> Result<Self> {
        let lock_path = paths.daemon_lock();
        Ok(Self { cfg, paths, store, secrets, handle: DaemonHandle::new(), started_at: Utc::now(), lock_path })
    }

    /// Run until `handle.stop()` or SIGINT/SIGTERM. `once` performs a single
    /// tick (used by tests and `run --once`).
    pub async fn run(mut self, once: bool) -> Result<()> {
        let _ = (&mut self, once);
        todo!("TODO(agent-budget): acquire fd-lock, write pid, loop: tick().await then sleep(tick_secs); heartbeat each tick")
    }

    /// One scheduling pass.
    pub async fn tick(&mut self) -> Result<()> {
        todo!("TODO(agent-budget)")
    }
}
