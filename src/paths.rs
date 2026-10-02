//! Filesystem locations used by powerqueue.
//!
//! We deliberately use XDG-style directories on every platform (`~/.config`,
//! `~/.local/share`, `~/.local/state`) because that is what CLI users expect,
//! and because the paths are short enough to show in diagnostics.
//! `POWERQUEUE_HOME` overrides all three roots (useful for tests and for
//! running several isolated instances).

use std::path::{Path, PathBuf};

use crate::APP_NAME;

/// Resolved directory layout for one powerqueue instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `~/.config/powerqueue`
    pub config_dir: PathBuf,
    /// `~/.local/share/powerqueue`
    pub data_dir: PathBuf,
    /// `~/.local/state/powerqueue`
    pub state_dir: PathBuf,
}

impl Paths {
    /// Resolve directories from the environment.
    pub fn resolve() -> Self {
        if let Some(home) = std::env::var_os("POWERQUEUE_HOME") {
            let home = PathBuf::from(home);
            return Self { config_dir: home.join("config"), data_dir: home.join("data"), state_dir: home.join("state") };
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        let xdg = |var: &str, default: &str| -> PathBuf {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(default))
                .join(APP_NAME)
        };
        Self {
            config_dir: xdg("XDG_CONFIG_HOME", ".config"),
            data_dir: xdg("XDG_DATA_HOME", ".local/share"),
            state_dir: xdg("XDG_STATE_HOME", ".local/state"),
        }
    }

    /// Build a layout rooted at a single directory (tests, `POWERQUEUE_HOME`).
    pub fn rooted(root: &Path) -> Self {
        Self { config_dir: root.join("config"), data_dir: root.join("data"), state_dir: root.join("state") }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn priority_file(&self) -> PathBuf {
        self.config_dir.join("PRIORITY.md")
    }
    pub fn secrets_file(&self) -> PathBuf {
        self.config_dir.join("secrets.toml")
    }
    pub fn database(&self) -> PathBuf {
        self.data_dir.join("powerqueue.db")
    }
    pub fn worktrees_dir(&self) -> PathBuf {
        self.data_dir.join("worktrees")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }
    pub fn log_file(&self) -> PathBuf {
        self.logs_dir().join("powerqueue.log")
    }
    pub fn tasks_dir(&self) -> PathBuf {
        self.state_dir.join("tasks")
    }
    /// Per-task scratch directory: launch script, prompt, hook settings, captured output.
    pub fn task_dir(&self, task_id: &str) -> PathBuf {
        self.tasks_dir().join(task_id)
    }
    pub fn daemon_lock(&self) -> PathBuf {
        self.state_dir.join("daemon.lock")
    }
    pub fn daemon_pid(&self) -> PathBuf {
        self.state_dir.join("daemon.pid")
    }

    /// Create every directory in the layout.
    pub fn ensure(&self) -> std::io::Result<()> {
        for dir in [&self.config_dir, &self.data_dir, &self.state_dir, &self.worktrees_dir(), &self.logs_dir(), &self.tasks_dir()]
        {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

/// Expand a leading `~` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if path == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooted_layout_is_predictable() {
        let p = Paths::rooted(Path::new("/tmp/pq"));
        assert_eq!(p.config_file(), PathBuf::from("/tmp/pq/config/config.toml"));
        assert_eq!(p.database(), PathBuf::from("/tmp/pq/data/powerqueue.db"));
        assert_eq!(p.task_dir("abc"), PathBuf::from("/tmp/pq/state/tasks/abc"));
    }

    #[test]
    fn tilde_expansion() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/x"), PathBuf::from(format!("{home}/x")));
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
    }
}
