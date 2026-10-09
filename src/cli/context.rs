//! Lazily-constructed shared state for command handlers.

use std::path::PathBuf;

use anyhow::Result;

use crate::config::Config;
use crate::logging::Verbosity;
use crate::paths::Paths;
use crate::secrets::Secrets;
use crate::store::Store;

/// Everything a command may need. Config and store are loaded on demand so
/// commands like `init` and `completions` work before anything exists.
pub struct Context {
    pub paths: Paths,
    /// `--home` / `POWERQUEUE_HOME`, when given.
    pub home: Option<PathBuf>,
    pub verbosity: Verbosity,
    pub json: bool,
    pub color: bool,
    config: Option<Config>,
    store: Option<Store>,
    secrets: Option<Secrets>,
    _log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

impl Context {
    pub fn new(home: Option<PathBuf>, verbosity: Verbosity, json: bool, color: bool) -> Self {
        let paths = match &home {
            Some(h) => Paths::rooted(h),
            None => Paths::resolve(),
        };
        Self { paths, home, verbosity, json, color, config: None, store: None, secrets: None, _log_guard: None }
    }

    /// Initialise file + stderr logging. `stderr=false` for TUI commands.
    pub fn init_logging(&mut self, stderr: bool) -> Result<()> {
        if self._log_guard.is_some() {
            return Ok(());
        }
        let logging = self.config_or_default()?.logging.clone();
        let guard = crate::logging::init(&self.paths, &logging, self.verbosity, stderr)?;
        self._log_guard = Some(guard);
        Ok(())
    }

    /// Config with repo overrides applied. Error if not initialised. A
    /// `.powerqueue.toml` that cannot be read or parsed does not fail the
    /// command: the global configuration is used, the problem is recorded in
    /// `overrides.error` (shown by `status`, `doctor` and `config show`) and
    /// warned about once on stderr, so the commands that diagnose or stop
    /// the daemon keep working. `powerqueue run` checks the field and
    /// refuses to start.
    pub fn config(&mut self) -> Result<&Config> {
        if self.config.is_none() {
            let cfg = Config::load(&self.paths)?;
            self.config = Some(self.with_repo_overrides(cfg));
        }
        Ok(self.config.as_ref().expect("config just loaded"))
    }

    /// Config, or defaults when no file exists yet. Repo overrides degrade
    /// as in [`Context::config`].
    pub fn config_or_default(&mut self) -> Result<&Config> {
        if self.config.is_none() {
            let cfg = Config::load_or_default(&self.paths)?;
            self.config = Some(self.with_repo_overrides(cfg));
        }
        Ok(self.config.as_ref().expect("config just loaded"))
    }

    fn with_repo_overrides(&self, mut cfg: Config) -> Config {
        let repo = cfg.repo_path();
        if !cfg.repo.path.trim().is_empty()
            && repo.exists()
            && let Some(error) = cfg.apply_repo_overrides_or_record(&repo)
        {
            eprintln!("warning: {error}; running with config.toml alone (see `powerqueue doctor`)");
        }
        cfg
    }

    pub fn config_cloned(&mut self) -> Result<Config> {
        Ok(self.config()?.clone())
    }

    pub fn store(&mut self) -> Result<&Store> {
        if self.store.is_none() {
            self.paths.ensure()?;
            self.store = Some(Store::open(&self.paths.database())?);
        }
        Ok(self.store.as_ref().expect("store just opened"))
    }

    pub fn secrets(&mut self) -> &Secrets {
        if self.secrets.is_none() {
            self.secrets = Some(Secrets::auto(&self.paths));
        }
        self.secrets.as_ref().expect("secrets just created")
    }

    pub fn is_initialised(&self) -> bool {
        self.paths.config_file().exists()
    }
}
