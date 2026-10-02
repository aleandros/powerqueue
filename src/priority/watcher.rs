//! Live reload of `PRIORITY.md`.
//!
//! Editors usually save by writing a temporary file and renaming it over the
//! original, which replaces the inode. Watching the file itself would
//! therefore stop working after the first save, so the parent directory is
//! watched and events are filtered to our file name. An mtime poll backs the
//! OS watcher up so a change is never missed, even when no watcher could be
//! created (network drives, exotic filesystems).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use notify::Watcher;
use tracing::{debug, warn};

/// Watches one file and raises a flag when it changes. Falls back to mtime
/// polling if the OS watcher cannot be created (network drives, etc.).
pub struct RulesWatcher {
    path: PathBuf,
    changed: Arc<AtomicBool>,
    _watcher: Option<notify::RecommendedWatcher>,
    last_mtime: Option<std::time::SystemTime>,
}

impl RulesWatcher {
    /// Start watching `path`. The file does not have to exist yet: its
    /// creation counts as a change. Never fails because of a missing watcher
    /// backend; that case degrades to polling and logs a warning.
    pub fn new(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        let changed = Arc::new(AtomicBool::new(false));
        let watcher = match Self::start_watcher(&path, Arc::clone(&changed)) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(target: "powerqueue::priority", file = %path.display(), error = %e, "file watcher unavailable; polling mtime instead");
                None
            }
        };
        let last_mtime = mtime(&path);
        Ok(Self { path, changed, _watcher: watcher, last_mtime })
    }

    fn start_watcher(path: &Path, flag: Arc<AtomicBool>) -> Result<notify::RecommendedWatcher> {
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let file_name =
            path.file_name().map(|n| n.to_os_string()).ok_or_else(|| anyhow::anyhow!("{} has no file name", path.display()))?;
        let target = path.to_path_buf();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                let ours = event.paths.iter().any(|p| p == &target || p.file_name() == Some(file_name.as_os_str()));
                if ours && !matches!(event.kind, notify::EventKind::Access(_)) {
                    debug!(target: "powerqueue::priority", kind = ?event.kind, "rules file changed");
                    flag.store(true, Ordering::SeqCst);
                }
            }
            Err(e) => warn!(target: "powerqueue::priority", error = %e, "file watcher error"),
        })?;
        watcher.watch(&dir, notify::RecursiveMode::NonRecursive)?;
        debug!(target: "powerqueue::priority", dir = %dir.display(), file = %path.display(), "watching rules file");
        Ok(watcher)
    }

    /// True once since the last call if the file changed (edge-triggered).
    /// Combines the watcher flag with an mtime comparison, so a change is
    /// reported even if the OS delivered no event.
    pub fn take_changed(&mut self) -> bool {
        let flagged = self.changed.swap(false, Ordering::SeqCst);
        let current = mtime(&self.path);
        let mtime_changed = current != self.last_mtime;
        self.last_mtime = current;
        flagged || mtime_changed
    }

    /// The watched file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for RulesWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RulesWatcher").field("path", &self.path).field("os_watcher", &self._watcher.is_some()).finish()
    }
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn reports_a_change_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("PRIORITY.md");
        let mut w = RulesWatcher::new(&path).unwrap();
        assert!(!w.take_changed(), "nothing happened yet");

        std::fs::write(&path, "## Low\n- label: chore\n").unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        assert!(w.take_changed(), "creation must be reported");
        assert!(!w.take_changed(), "edge-triggered: second call is false");

        // Atomic replace, as editors do it.
        let tmp = dir.path().join("PRIORITY.md.tmp");
        std::fs::write(&tmp, "## High\n- label: x\n").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        std::fs::rename(&tmp, &path).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        assert!(w.take_changed(), "rename over the file must be reported");
        assert!(!w.take_changed());
    }

    #[test]
    fn tolerates_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("PRIORITY.md");
        let mut w = RulesWatcher::new(&path).unwrap();
        assert!(!w.take_changed());
        assert_eq!(w.path(), path.as_path());
    }
}
