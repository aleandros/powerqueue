//! Live reload of `PRIORITY.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

/// Watches one file and raises a flag when it changes. Falls back to mtime
/// polling if the OS watcher cannot be created (network drives, etc.).
pub struct RulesWatcher {
    path: PathBuf,
    changed: Arc<AtomicBool>,
    _watcher: Option<notify::RecommendedWatcher>,
    last_mtime: Option<std::time::SystemTime>,
}

impl RulesWatcher {
    pub fn new(path: &Path) -> Result<Self> {
        let _ = path;
        todo!("TODO(agent-linear): notify watcher on parent dir filtered to `path`")
    }

    /// True once since the last call if the file changed (edge-triggered).
    pub fn take_changed(&mut self) -> bool {
        let _ = (&self.path, &self.changed, &mut self.last_mtime);
        let _ = Ordering::SeqCst;
        todo!("TODO(agent-linear): combine notify flag with mtime poll")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
