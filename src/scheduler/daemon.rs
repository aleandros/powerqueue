//! Daemon state and main loop.
//!
//! The loop is deliberately boring: every phase of [`Daemon::tick`] reads
//! rows from the store, asks the pure functions in [`super::transitions`]
//! what should happen, persists the result and applies the returned
//! [`Effect`]s against tmux, git and Linear. A failure in one phase or one
//! task is logged as an event and never stops the other phases.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Duration, Utc};
use tokio::sync::Notify;

use crate::budget::{Decision, Estimator, Ledger, PeriodClock, Policy, RATE_LIMITS_KEY, RateLimitState, tier_weight};
use crate::config::Config;
use crate::domain::{DaemonCommand, EventLevel, ModelTier, Session, SessionState, Task, TaskId, TaskState};
use crate::jev::{JevClient, JevQuestion, content_hash};
use crate::linear::{IssueFilter, LinearClient, sync_issues};
use crate::paths::Paths;
use crate::priority::{PriorityRules, RulesWatcher};
use crate::secrets::{SecretKind, Secrets};
use crate::session::{
    Launcher, TranscriptReader, interpret_hook, probe_session, sample_resources, transcript::claude_home, transcript_path_for,
};
use crate::store::{JevCached, PendingHookEvent, Store};
use crate::tmux::Tmux;
use crate::worktree::{Repo, branch_name, run_commands};

use super::lifecycle::{cleanup_task, pick_next, worktree_dir};
use super::transitions::{self, CRASH_TAIL_LINES, Effect, LinearTarget, ProbeContext};

/// `last_error` marker for tasks paused by a PRIORITY.md `skip` override.
pub const SKIP_REASON: &str = "skipped by PRIORITY.md";
/// Longest pause between Linear polls after repeated failures.
const LINEAR_MAX_BACKOFF: Duration = Duration::minutes(10);
/// How long resource samples are kept.
const RESOURCE_RETENTION: Duration = Duration::days(7);
/// Ended sessions keep having their transcript tailed for this long.
const TRANSCRIPT_GRACE: Duration = Duration::minutes(5);

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

/// Mutable runtime state that is not part of the public daemon surface.
struct RuntimeState {
    tmux: Tmux,
    repo: Repo,
    launcher: Launcher,
    rules: PriorityRules,
    rules_loaded: bool,
    rules_missing_logged: bool,
    watcher: Option<RulesWatcher>,
    watcher_failed: bool,
    readers: HashMap<uuid::Uuid, TranscriptReader>,
    nudged: HashSet<uuid::Uuid>,
    rate_limits: RateLimitState,
    linear: Option<LinearClient>,
    linear_warned: bool,
    last_linear_poll: Option<DateTime<Utc>>,
    linear_backoff: Duration,
    linear_next_allowed: Option<DateTime<Utc>>,
    force_sync: bool,
    jev: Option<JevClient>,
    jev_error_logged: bool,
    last_resource_sample: Option<DateTime<Utc>>,
    last_prune: Option<DateTime<Utc>>,
    system: sysinfo::System,
    shutdown: bool,
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
    /// Never talk to Linear (`run --offline`).
    pub offline: bool,
    rt: RuntimeState,
}

impl Daemon {
    /// Build a daemon. Fails when the running executable cannot be located
    /// (needed for hook commands). Does not touch tmux, git or the network.
    pub fn new(cfg: Config, paths: Paths, store: Store, secrets: Secrets) -> Result<Self> {
        let lock_path = paths.daemon_lock();
        let tmux = Tmux::new(&cfg.tmux.binary, cfg.tmux.socket_name.clone());
        let repo = Repo::new(cfg.repo_path());
        let launcher = Launcher::new(paths.clone(), tmux.clone()).context("locate the powerqueue executable")?;
        let rate_limits = store.kv_get::<RateLimitState>(RATE_LIMITS_KEY).ok().flatten().unwrap_or_default();
        let rt = RuntimeState {
            tmux,
            repo,
            launcher,
            rules: PriorityRules::default(),
            rules_loaded: false,
            rules_missing_logged: false,
            watcher: None,
            watcher_failed: false,
            readers: HashMap::new(),
            nudged: HashSet::new(),
            rate_limits,
            linear: None,
            linear_warned: false,
            last_linear_poll: None,
            linear_backoff: Duration::zero(),
            linear_next_allowed: None,
            force_sync: false,
            jev: None,
            jev_error_logged: false,
            last_resource_sample: None,
            last_prune: None,
            system: sysinfo::System::new(),
            shutdown: false,
        };
        Ok(Self {
            cfg,
            paths,
            store,
            secrets,
            handle: DaemonHandle::new(),
            started_at: Utc::now(),
            lock_path,
            offline: false,
            rt,
        })
    }

    /// Run until `handle.stop()` or SIGINT/SIGTERM. `once` performs a single
    /// tick (used by tests and `run --once`).
    ///
    /// Takes an exclusive lock on `daemon.lock`; fails with "another daemon is
    /// running (pid …)" when it is held. Writes `daemon.pid` for the duration.
    pub async fn run(mut self, once: bool) -> Result<()> {
        self.paths.ensure().with_context(|| format!("create {}", self.paths.state_dir.display()))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .with_context(|| format!("open lock file {}", self.lock_path.display()))?;
        let mut lock = fd_lock::RwLock::new(file);
        let _guard = match lock.try_write() {
            Ok(guard) => guard,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let pid = std::fs::read_to_string(self.paths.daemon_pid())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| "unknown".into());
                bail!("another daemon is running (pid {pid}); stop it with `powerqueue stop`");
            }
            Err(e) => return Err(e).with_context(|| format!("lock {}", self.lock_path.display())),
        };
        let pid = std::process::id();
        std::fs::write(self.paths.daemon_pid(), pid.to_string())
            .with_context(|| format!("write {}", self.paths.daemon_pid().display()))?;
        tracing::info!(pid, once, repo = %self.cfg.repo.path, "daemon started");
        self.log(
            None,
            None,
            EventLevel::Info,
            "daemon.started",
            &format!("daemon started (pid {pid})"),
            serde_json::json!({ "pid": pid, "once": once }),
        );

        let notify = Arc::new(Notify::new());
        {
            let handle = self.handle.clone();
            let notify = notify.clone();
            tokio::spawn(async move {
                wait_for_signal().await;
                tracing::info!("shutdown signal received");
                handle.stop();
                notify.notify_one();
            });
        }

        let result = self.main_loop(once, &notify).await;
        self.log(None, None, EventLevel::Info, "daemon.stopped", "daemon stopped", serde_json::json!({ "pid": pid }));
        let _ = std::fs::remove_file(self.paths.daemon_pid());
        tracing::info!("daemon stopped");
        result
    }

    async fn main_loop(&mut self, once: bool, notify: &Notify) -> Result<()> {
        loop {
            let started = std::time::Instant::now();
            if let Err(e) = self.tick().await {
                tracing::error!(error = %format!("{e:#}"), "tick failed");
                self.log(None, None, EventLevel::Error, "daemon.error", &format!("tick failed: {e:#}"), serde_json::json!({}));
            }
            if once || self.rt.shutdown || self.handle.should_stop() {
                return Ok(());
            }
            let period = std::time::Duration::from_secs(self.cfg.scheduler.tick_secs.max(1));
            let sleep = period.saturating_sub(started.elapsed());
            tokio::select! {
                _ = tokio::time::sleep(sleep) => {}
                _ = notify.notified() => {}
            }
            if self.handle.should_stop() {
                return Ok(());
            }
        }
    }

    /// One scheduling pass. Each phase is isolated: its error is logged as a
    /// `daemon.error` event and the next phase still runs.
    pub async fn tick(&mut self) -> Result<()> {
        let now = Utc::now();
        self.store.heartbeat(std::process::id()).context("write heartbeat")?;
        let r = self.apply_commands(now).await;
        self.report_phase("commands", r);
        if self.rt.shutdown {
            return Ok(());
        }
        let r = self.refresh_rules(now).await;
        self.report_phase("rules", r);
        let r = self.poll_linear(now).await;
        self.report_phase("linear", r);
        let r = self.process_hooks(now).await;
        self.report_phase("hooks", r);
        let r = self.tail_transcripts(now);
        self.report_phase("transcripts", r);
        let r = self.probe_sessions(now).await;
        self.report_phase("probes", r);
        let r = self.finalize_terminal(now).await;
        self.report_phase("finalize", r);
        let r = self.sample_resources(now);
        self.report_phase("resources", r);
        let r = self.launch_tasks(now).await;
        self.report_phase("launch", r);
        Ok(())
    }

    fn report_phase(&self, phase: &str, result: Result<()>) {
        if let Err(e) = result {
            tracing::error!(phase, error = %format!("{e:#}"), "phase failed");
            self.log(
                None,
                None,
                EventLevel::Error,
                "daemon.error",
                &format!("{phase}: {e:#}"),
                serde_json::json!({ "phase": phase }),
            );
        }
    }

    /// Persist a timeline event and mirror it to tracing. Store failures are
    /// only traced: logging must never take the daemon down.
    fn log(
        &self,
        task: Option<TaskId>,
        session: Option<uuid::Uuid>,
        level: EventLevel,
        kind: &str,
        message: &str,
        data: serde_json::Value,
    ) {
        match level {
            EventLevel::Debug => tracing::debug!(kind, task = ?task.map(|t| t.short()), "{message}"),
            EventLevel::Info => tracing::info!(kind, task = ?task.map(|t| t.short()), "{message}"),
            EventLevel::Warn => tracing::warn!(kind, task = ?task.map(|t| t.short()), "{message}"),
            EventLevel::Error => tracing::error!(kind, task = ?task.map(|t| t.short()), "{message}"),
        }
        if let Err(e) = self.store.log_event(task, session, level, kind, message, data) {
            tracing::warn!(kind, error = %e, "could not persist event");
        }
    }

    fn clock(&self, now: DateTime<Utc>) -> PeriodClock {
        PeriodClock::from_config(&self.cfg.budget, now)
    }

    // ------------------------------------------------------------ commands

    async fn apply_commands(&mut self, now: DateTime<Utc>) -> Result<()> {
        for cmd in self.store.drain_commands()? {
            tracing::debug!(?cmd, "applying daemon command");
            if let Err(e) = self.apply_command(&cmd, now).await {
                tracing::warn!(?cmd, error = %format!("{e:#}"), "daemon command failed");
                self.log(
                    None,
                    None,
                    EventLevel::Warn,
                    "daemon.command_failed",
                    &format!("{cmd:?}: {e:#}"),
                    serde_json::json!({}),
                );
            }
        }
        Ok(())
    }

    async fn apply_command(&mut self, cmd: &DaemonCommand, now: DateTime<Utc>) -> Result<()> {
        match cmd {
            DaemonCommand::Pause { task_id } => {
                let mut task = self.require_task(*task_id)?;
                if task.state.is_terminal() || task.state == TaskState::Paused {
                    return Ok(());
                }
                task.state = TaskState::Paused;
                task.not_before = None;
                self.store.update_task(&task)?;
                let msg = if self.store.latest_session(task.id)?.is_some_and(|s| s.state.is_live()) {
                    "paused; the current session finishes its turn and will not be relaunched"
                } else {
                    "paused"
                };
                self.log(Some(task.id), None, EventLevel::Info, "task.paused", msg, serde_json::json!({}));
            }
            DaemonCommand::Resume { task_id } => {
                let mut task = self.require_task(*task_id)?;
                if !matches!(task.state, TaskState::Paused | TaskState::NeedsAttention) {
                    return Ok(());
                }
                let live = self.store.latest_session(task.id)?.filter(|s| s.state.is_live());
                task.not_before = None;
                if task.last_error.as_deref() == Some(SKIP_REASON) {
                    task.last_error = None;
                }
                task.state = if live.is_some() { TaskState::Running } else { TaskState::Queued };
                self.store.update_task(&task)?;
                self.log(
                    Some(task.id),
                    live.map(|s| s.id),
                    EventLevel::Info,
                    "task.resumed",
                    &format!("resumed → {}", task.state),
                    serde_json::json!({}),
                );
            }
            DaemonCommand::Cancel { task_id } => {
                let mut task = self.require_task(*task_id)?;
                if task.state.is_terminal() {
                    return Ok(());
                }
                if let Some(mut session) = self.store.latest_session(task.id)?.filter(|s| s.state.is_live()) {
                    if let Err(e) = self.rt.tmux.kill_window(&session.tmux_window) {
                        tracing::debug!(task = %task.key, error = %format!("{e:#}"), "kill window on cancel");
                    }
                    session.state = SessionState::Killed;
                    session.ended_at = Some(now);
                    self.store.update_session(&session)?;
                    self.forget_session(session.id);
                }
                task.state = TaskState::Cancelled;
                task.completed_at = Some(now);
                task.not_before = None;
                self.store.update_task(&task)?;
                self.log(Some(task.id), None, EventLevel::Info, "task.cancelled", "cancelled by user", serde_json::json!({}));
                self.apply_effects(&mut task, None, vec![Effect::Cleanup { succeeded: false }]).await;
                self.store.update_task(&task)?;
            }
            DaemonCommand::Retry { task_id } => {
                let mut task = self.require_task(*task_id)?;
                if task.state.is_terminal() {
                    task.attempts = 0;
                    task.last_error = None;
                    task.summary = None;
                    task.completed_at = None;
                    task.started_at = None;
                } else if !matches!(
                    task.state,
                    TaskState::Crashed | TaskState::Throttled | TaskState::Paused | TaskState::NeedsAttention
                ) {
                    return Ok(());
                }
                task.state = TaskState::Queued;
                task.not_before = None;
                self.store.update_task(&task)?;
                self.log(Some(task.id), None, EventLevel::Info, "task.retried", "re-queued by user", serde_json::json!({}));
            }
            DaemonCommand::SetModel { task_id, model } => {
                let mut task = self.require_task(*task_id)?;
                task.model_override = *model;
                self.store.update_task(&task)?;
                let msg = match model {
                    Some(m) => format!("model forced to {m} for the next attempt"),
                    None => "model override cleared".to_string(),
                };
                self.log(
                    Some(task.id),
                    None,
                    EventLevel::Info,
                    "task.model_override",
                    &msg,
                    serde_json::json!({ "model": model }),
                );
            }
            DaemonCommand::SyncNow => self.rt.force_sync = true,
            DaemonCommand::Reload => {
                self.reload_config()?;
                self.log(
                    None,
                    None,
                    EventLevel::Info,
                    "daemon.reloaded",
                    "configuration and rules reloaded",
                    serde_json::json!({}),
                );
            }
            DaemonCommand::Shutdown => {
                self.rt.shutdown = true;
                self.handle.stop();
                tracing::info!("shutdown requested");
            }
        }
        Ok(())
    }

    fn require_task(&self, id: TaskId) -> Result<Task> {
        self.store.get_task(id)?.ok_or_else(|| anyhow::anyhow!("task {id} not found"))
    }

    fn reload_config(&mut self) -> Result<()> {
        let mut cfg = Config::load(&self.paths)?;
        let repo = cfg.repo_path();
        if repo.exists() {
            cfg.apply_repo_overrides(&repo)?;
        }
        cfg.ensure_valid()?;
        self.rt.tmux = Tmux::new(&cfg.tmux.binary, cfg.tmux.socket_name.clone());
        self.rt.repo = Repo::new(cfg.repo_path());
        self.rt.launcher = Launcher::new(self.paths.clone(), self.rt.tmux.clone())?;
        self.rt.linear = None;
        self.rt.linear_warned = false;
        self.rt.jev = None;
        self.rt.rules_loaded = false;
        self.rt.watcher = None;
        self.rt.watcher_failed = false;
        self.cfg = cfg;
        Ok(())
    }

    // --------------------------------------------------------------- rules

    async fn refresh_rules(&mut self, now: DateTime<Utc>) -> Result<()> {
        let path = self.cfg.priority_file(&self.paths);
        if self.rt.watcher.is_none() && !self.rt.watcher_failed && self.cfg.priority.live_reload {
            match RulesWatcher::new(&path) {
                Ok(w) => self.rt.watcher = Some(w),
                Err(e) => {
                    self.rt.watcher_failed = true;
                    tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "cannot watch PRIORITY.md; use `powerqueue run` restart or Reload to pick up changes");
                }
            }
        }
        let changed = !self.rt.rules_loaded || self.rt.watcher.as_mut().is_some_and(|w| w.take_changed());
        if changed {
            self.load_rules(&path);
        }

        let tasks = self.store.list_open_tasks()?;
        for mut task in tasks.into_iter().filter(|t| !t.state.has_live_session()) {
            let jev_norm = self.jev_norm_for(&task, now).await;
            let eval =
                self.rt.rules.evaluate(&task, now, jev_norm, self.cfg.priority.jev.weight, self.cfg.priority.age_boost_per_hour);
            let mut changed = false;
            if task.criticality != eval.criticality {
                task.criticality = eval.criticality;
                changed = true;
            }
            if (task.score - eval.score).abs() > 0.5 {
                task.score = eval.score;
                changed = true;
            }
            if changed {
                task.score_reasons = eval.reasons.clone();
            }
            if eval.skip && task.state != TaskState::Paused && !task.state.is_terminal() {
                task.state = TaskState::Paused;
                task.last_error = Some(SKIP_REASON.to_string());
                task.not_before = None;
                changed = true;
                self.log(Some(task.id), None, EventLevel::Info, "task.skipped", SKIP_REASON, serde_json::json!({}));
            } else if !eval.skip && task.state == TaskState::Paused && task.last_error.as_deref() == Some(SKIP_REASON) {
                task.state = TaskState::Queued;
                task.last_error = None;
                changed = true;
                self.log(
                    Some(task.id),
                    None,
                    EventLevel::Info,
                    "task.unskipped",
                    "no longer skipped by PRIORITY.md",
                    serde_json::json!({}),
                );
            }
            if changed {
                self.store.update_task(&task)?;
                tracing::debug!(task = %task.key, criticality = %task.criticality, score = task.score, "re-scored");
            }
        }
        Ok(())
    }

    fn load_rules(&mut self, path: &Path) {
        self.rt.rules_loaded = true;
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                if !self.rt.rules_missing_logged {
                    self.rt.rules_missing_logged = true;
                    self.log(
                        None,
                        None,
                        EventLevel::Warn,
                        "rules.missing",
                        &format!("cannot read {} ({e}); using default rules", path.display()),
                        serde_json::json!({ "path": path }),
                    );
                }
                self.rt.rules = PriorityRules::default();
                return;
            }
        };
        match PriorityRules::parse(&text) {
            Ok(rules) => {
                let warnings: Vec<String> = rules.warnings.iter().map(|w| w.to_string()).collect();
                self.log(
                    None,
                    None,
                    EventLevel::Info,
                    "rules.loaded",
                    &format!("loaded {} ({} warning(s))", path.display(), warnings.len()),
                    serde_json::json!({ "path": path, "warnings": warnings }),
                );
                self.rt.rules = rules;
            }
            Err(errors) => {
                let errors: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
                self.log(
                    None,
                    None,
                    EventLevel::Warn,
                    "rules.invalid",
                    &format!("{} has errors; keeping the previous rules: {}", path.display(), errors.join("; ")),
                    serde_json::json!({ "path": path, "errors": errors }),
                );
            }
        }
    }

    /// Jev score for a task normalised to 0..=1, from cache or by scoring.
    async fn jev_norm_for(&mut self, task: &Task, now: DateTime<Utc>) -> Option<f64> {
        let levels = self.rt.rules.jev.levels.len().max(2);
        let normalise = |score: f64| (score / (levels as f64 - 1.0)).clamp(0.0, 1.0);
        let hash = content_hash(&task.title, &task.description, &task.labels);
        let cached = self.store.jev_cached(task.id).ok().flatten();
        let enabled = self.cfg.priority.jev.enabled && self.rt.rules.jev.enabled;
        if let Some(c) = &cached
            && (!enabled || c.content_hash == hash || !self.cfg.priority.jev.rescore_on_change)
        {
            return Some(normalise(c.score));
        }
        if !enabled {
            return None;
        }
        if self.rt.jev.is_none() {
            match self.secrets.get(SecretKind::JevApiKey) {
                Ok(Some(key)) => match JevClient::new(&self.cfg.priority.jev.endpoint, key, &self.cfg.priority.jev.model) {
                    Ok(c) => self.rt.jev = Some(c),
                    Err(e) => self.jev_error_once(&format!("cannot build Jev client: {e:#}")),
                },
                Ok(None) => self.jev_error_once("Jev scoring is enabled but no Jev API key is configured"),
                Err(e) => self.jev_error_once(&format!("cannot read the Jev API key: {e:#}")),
            }
        }
        let client = self.rt.jev.clone()?;
        let question = JevQuestion { instructions: self.rt.rules.jev.question.clone(), levels: self.rt.rules.jev.levels.clone() };
        let state = crate::jev::task_state(task);
        match client.score(&state, &question).await {
            Ok(score) => {
                let cached = JevCached {
                    content_hash: hash,
                    score: score.score,
                    level: score.level.clone(),
                    confidence: score.confidence,
                    raw: score.raw.to_string(),
                    scored_at: now.to_rfc3339(),
                };
                if let Err(e) = self.store.jev_store(task.id, &cached) {
                    tracing::warn!(task = %task.key, error = %e, "cannot cache Jev score");
                }
                self.log(
                    Some(task.id),
                    None,
                    EventLevel::Debug,
                    "jev.scored",
                    &format!("Jev: {} ({:.2}, confidence {:.2})", score.level, score.score, score.confidence),
                    serde_json::json!({ "score": score.score, "level": score.level, "confidence": score.confidence }),
                );
                Some(score.normalized())
            }
            Err(e) => {
                self.jev_error_once(&format!("Jev scoring failed: {e:#}"));
                cached.map(|c| normalise(c.score))
            }
        }
    }

    fn jev_error_once(&mut self, message: &str) {
        if !self.rt.jev_error_logged {
            self.rt.jev_error_logged = true;
            self.log(None, None, EventLevel::Warn, "jev.error", message, serde_json::json!({}));
        }
    }

    // -------------------------------------------------------------- linear

    /// The Linear client, built on first use. `None` (with a one-time warning)
    /// when Linear is disabled, offline, or no API key is configured.
    fn linear_client(&mut self) -> Option<LinearClient> {
        if self.offline || !self.cfg.linear.enabled {
            return None;
        }
        if self.rt.linear.is_none() {
            let built = match self.secrets.get(SecretKind::LinearApiKey) {
                Ok(Some(key)) => LinearClient::new(&self.cfg.linear.endpoint, key).map(Some),
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            };
            match built {
                Ok(Some(c)) => self.rt.linear = Some(c),
                Ok(None) => self.linear_warn_once(
                    "Linear is enabled but no API key is configured (run `powerqueue secrets set linear`); continuing offline",
                ),
                Err(e) => self.linear_warn_once(&format!("cannot build the Linear client: {e:#}; continuing offline")),
            }
        }
        self.rt.linear.clone()
    }

    fn linear_warn_once(&mut self, message: &str) {
        if !self.rt.linear_warned {
            self.rt.linear_warned = true;
            self.log(None, None, EventLevel::Warn, "linear.unavailable", message, serde_json::json!({}));
        }
    }

    async fn poll_linear(&mut self, now: DateTime<Utc>) -> Result<()> {
        if self.offline || !self.cfg.linear.enabled {
            return Ok(());
        }
        let interval = Duration::seconds(self.cfg.linear.poll_interval_secs.max(5) as i64);
        let due = self.rt.force_sync || self.rt.last_linear_poll.is_none_or(|t| now - t >= interval);
        let backing_off = self.rt.linear_next_allowed.is_some_and(|t| now < t) && !self.rt.force_sync;
        if !due || backing_off {
            return Ok(());
        }
        let Some(client) = self.linear_client() else { return Ok(()) };
        self.rt.force_sync = false;
        self.rt.last_linear_poll = Some(now);

        let lc = &self.cfg.linear;
        let filter = IssueFilter {
            team_keys: lc.team_keys.clone(),
            assignee: lc.assignee.clone(),
            state_names: lc.queued_states.clone(),
            required_labels: lc.required_labels.clone(),
            excluded_labels: lc.excluded_labels.clone(),
            max: lc.max_issues,
        };
        let issues = match client.fetch_issues(&filter).await {
            Ok(issues) => issues,
            Err(e) => {
                self.linear_failure(now, &format!("fetching issues failed: {e:#}"));
                return Ok(());
            }
        };

        // `sync_issues` takes a synchronous state lookup, so fetch the states
        // of open tasks whose issues are no longer in the queued set up front.
        let present: HashSet<&str> = issues.iter().map(|i| i.id.as_str()).collect();
        let mut states: HashMap<String, String> = HashMap::new();
        let open = self.store.list_open_tasks()?;
        for task in open
            .iter()
            .filter(|t| matches!(t.state, TaskState::Queued | TaskState::Throttled | TaskState::Paused | TaskState::Crashed))
        {
            let Some(issue_id) = task.linear_issue_id() else { continue };
            if present.contains(issue_id) {
                continue;
            }
            match client.get_issue(issue_id).await {
                Ok(Some(issue)) => {
                    states.insert(issue_id.to_string(), issue.state_type);
                }
                Ok(None) => {
                    states.insert(issue_id.to_string(), "canceled".to_string());
                }
                Err(e) => tracing::warn!(task = %task.key, error = %format!("{e:#}"), "cannot fetch issue state"),
            }
        }

        let report = sync_issues(&self.store, &self.cfg.linear, &issues, |id| states.get(id).cloned())?;
        self.rt.linear_backoff = Duration::zero();
        self.rt.linear_next_allowed = None;
        if !report.created.is_empty() || !report.updated.is_empty() || !report.cancelled.is_empty() {
            self.log(
                None,
                None,
                EventLevel::Info,
                "linear.sync",
                &format!(
                    "Linear sync: {} created, {} updated, {} cancelled, {} unchanged",
                    report.created.len(),
                    report.updated.len(),
                    report.cancelled.len(),
                    report.unchanged
                ),
                serde_json::json!({
                    "created": report.created,
                    "updated": report.updated,
                    "cancelled": report.cancelled,
                    "unchanged": report.unchanged,
                    "fetched": issues.len(),
                }),
            );
            for id in &report.created {
                self.log(Some(*id), None, EventLevel::Info, "task.created", "created from Linear", serde_json::json!({}));
            }
            for id in &report.cancelled {
                self.log(Some(*id), None, EventLevel::Info, "task.cancelled", "issue closed in Linear", serde_json::json!({}));
            }
        } else {
            tracing::debug!(fetched = issues.len(), "Linear sync: nothing changed");
        }
        Ok(())
    }

    fn linear_failure(&mut self, now: DateTime<Utc>, message: &str) {
        let base = Duration::seconds(self.cfg.linear.poll_interval_secs.max(5) as i64);
        self.rt.linear_backoff =
            if self.rt.linear_backoff.is_zero() { base } else { (self.rt.linear_backoff * 2).min(LINEAR_MAX_BACKOFF) };
        self.rt.linear_next_allowed = Some(now + self.rt.linear_backoff);
        self.log(
            None,
            None,
            EventLevel::Warn,
            "linear.error",
            &format!("{message}; next poll in {}s", self.rt.linear_backoff.num_seconds()),
            serde_json::json!({ "backoff_secs": self.rt.linear_backoff.num_seconds() }),
        );
    }

    /// Best-effort Linear update: move the issue (when a state is configured
    /// for `target`) and post `comment` (when comments are enabled).
    async fn update_linear(&mut self, task: &Task, target: LinearTarget, comment: Option<String>) {
        let Some(issue_id) = task.linear_issue_id().map(str::to_string) else { return };
        let Some(client) = self.linear_client() else { return };
        let state = match target {
            LinearTarget::InProgress => self.cfg.linear.in_progress_state.clone(),
            LinearTarget::Done => self.cfg.linear.done_state.clone(),
            LinearTarget::Blocked => self.cfg.linear.blocked_state.clone(),
        };
        if let Some(name) = state {
            match client.set_state(&issue_id, &name).await {
                Ok(()) => self.log(
                    Some(task.id),
                    None,
                    EventLevel::Info,
                    "linear.state",
                    &format!("moved issue to {name}"),
                    serde_json::json!({ "state": name }),
                ),
                Err(e) => self.log(
                    Some(task.id),
                    None,
                    EventLevel::Warn,
                    "linear.error",
                    &format!("could not move issue to {name}: {e:#}"),
                    serde_json::json!({ "state": name }),
                ),
            }
        }
        if self.cfg.linear.post_comments
            && let Some(body) = comment
        {
            match client.comment(&issue_id, &body).await {
                Ok(()) => self.log(
                    Some(task.id),
                    None,
                    EventLevel::Debug,
                    "linear.comment",
                    "posted comment",
                    serde_json::json!({ "body": body }),
                ),
                Err(e) => self.log(
                    Some(task.id),
                    None,
                    EventLevel::Warn,
                    "linear.error",
                    &format!("could not post comment: {e:#}"),
                    serde_json::json!({}),
                ),
            }
        }
    }

    // ---------------------------------------------------------------- hooks

    async fn process_hooks(&mut self, now: DateTime<Utc>) -> Result<()> {
        let events = self.store.drain_hook_events()?;
        if events.is_empty() {
            return Ok(());
        }
        let period_end = self.clock(now).current_period(now).end;
        for ev in events {
            if let Err(e) = self.process_hook(&ev, now, period_end).await {
                tracing::warn!(task = %ev.task_id, event = ev.event.as_str(), error = %format!("{e:#}"), "hook processing failed");
                self.log(
                    Some(ev.task_id),
                    ev.session_id,
                    EventLevel::Error,
                    "task.error",
                    &format!("hook {}: {e:#}", ev.event.as_str()),
                    serde_json::json!({}),
                );
            }
        }
        Ok(())
    }

    async fn process_hook(&mut self, ev: &PendingHookEvent, now: DateTime<Utc>, period_end: DateTime<Utc>) -> Result<()> {
        let Some(mut task) = self.store.get_task(ev.task_id)? else {
            tracing::debug!(task = %ev.task_id, "hook for unknown task ignored");
            return Ok(());
        };
        let mut session = match ev.session_id {
            Some(id) => self.store.get_session(id)?,
            None => None,
        };
        if session.is_none() {
            session = self.store.latest_session(task.id)?;
        }
        let Some(mut session) = session else {
            self.log(
                Some(task.id),
                ev.session_id,
                EventLevel::Debug,
                "hook.orphan",
                "hook event without a session",
                serde_json::json!({ "event": ev.event.as_str() }),
            );
            return Ok(());
        };
        let outcome = interpret_hook(ev.event, &ev.payload);
        tracing::debug!(task = %task.key, session = %session.id, event = ev.event.as_str(), ?outcome, "hook outcome");
        let effects = transitions::on_hook_outcome(&mut task, &mut session, &outcome, &self.cfg, now, period_end);
        self.store.update_session(&session)?;
        self.store.update_task(&task)?;
        self.apply_effects(&mut task, Some(&session), effects).await;
        self.store.update_task(&task)?;
        if !session.state.is_live() {
            self.forget_session(session.id);
        }
        Ok(())
    }

    /// Carry out effects requested by a transition. Runtime failures are
    /// logged; they never propagate.
    async fn apply_effects(&mut self, task: &mut Task, session: Option<&Session>, effects: Vec<Effect>) {
        let session_id = session.map(|s| s.id);
        for effect in effects {
            match effect {
                Effect::Log { level, kind, message, data } => self.log(Some(task.id), session_id, level, &kind, &message, data),
                Effect::Cleanup { succeeded } => {
                    if let Err(e) = cleanup_task(&self.cfg, &self.store, &self.rt.repo, &self.rt.tmux, task, succeeded) {
                        self.log(
                            Some(task.id),
                            session_id,
                            EventLevel::Warn,
                            "cleanup.error",
                            &format!("cleanup failed: {e:#}"),
                            serde_json::json!({}),
                        );
                    }
                    if let Some(id) = session_id {
                        self.forget_session(id);
                    }
                }
                Effect::Linear { target, comment } => self.update_linear(task, target, comment).await,
                Effect::Nudge { text } => {
                    if let Some(s) = session {
                        match s.pane_id.as_deref() {
                            Some(pane) => {
                                if let Err(e) = self.rt.tmux.send_text(pane, &text) {
                                    self.log(
                                        Some(task.id),
                                        session_id,
                                        EventLevel::Warn,
                                        "session.nudge_failed",
                                        &format!("{e:#}"),
                                        serde_json::json!({}),
                                    );
                                }
                            }
                            None => tracing::debug!(task = %task.key, "no pane id; cannot nudge"),
                        }
                        self.rt.nudged.insert(s.id);
                    }
                }
                Effect::KillWindow => {
                    if let Some(s) = session
                        && let Err(e) = self.rt.tmux.kill_window(&s.tmux_window)
                    {
                        tracing::debug!(task = %task.key, window = %s.tmux_window, error = %format!("{e:#}"), "kill window");
                    }
                }
                Effect::RateLimit { tier, until } => {
                    self.rt.rate_limits.mark(tier, until);
                    if let Err(e) = self.store.kv_set(RATE_LIMITS_KEY, &self.rt.rate_limits) {
                        tracing::warn!(error = %e, "cannot persist rate limits");
                    }
                }
            }
        }
    }

    fn forget_session(&mut self, id: uuid::Uuid) {
        // Readers are kept until the transcript grace period ends (see
        // `tail_transcripts`), so a session's final usage is never lost.
        self.rt.nudged.remove(&id);
    }

    // ---------------------------------------------------------- transcripts

    fn tail_transcripts(&mut self, now: DateTime<Utc>) -> Result<()> {
        // Sessions that ended within the grace window are swept too: the final
        // turn's usage lands in the transcript right before the Stop hook fires,
        // and the hook phase (which runs first) may already have closed the session.
        let active = self.store.list_sessions_active_since(now - TRANSCRIPT_GRACE)?;
        let keep: HashSet<uuid::Uuid> = active.iter().map(|s| s.id).collect();
        self.rt.readers.retain(|id, _| keep.contains(id));
        for mut session in active {
            let path = match &session.transcript_path {
                Some(p) => PathBuf::from(p),
                None => {
                    let Some(task) = self.store.get_task(session.task_id)? else { continue };
                    let Some(wt) = &task.worktree_path else { continue };
                    let guess = transcript_path_for(&claude_home(), Path::new(wt), session.id);
                    if !guess.exists() {
                        continue;
                    }
                    session.transcript_path = Some(guess.display().to_string());
                    self.store.update_session(&session)?;
                    guess
                }
            };
            let reader =
                self.rt.readers.entry(session.id).or_insert_with(|| TranscriptReader::new(path, session.id, session.task_id));
            let records = match reader.read_new() {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(session = %session.id, error = %format!("{e:#}"), "transcript read failed");
                    continue;
                }
            };
            if records.is_empty() {
                continue;
            }
            let mut inserted = 0usize;
            let mut weighted = 0.0;
            for rec in &records {
                if self.store.record_usage(rec)? {
                    inserted += 1;
                    weighted += rec.usage.weighted() * tier_weight(&self.cfg.budget, rec.tier);
                }
            }
            let newest = reader.last_line_at.unwrap_or(now).min(now);
            if session.state.is_live() {
                session.last_activity_at = session.last_activity_at.max(newest);
                if session.state == SessionState::Launching {
                    session.state = SessionState::Running;
                }
                self.store.update_session(&session)?;
            }
            tracing::debug!(session = %session.id, inserted, weighted, "recorded transcript usage");
        }
        Ok(())
    }

    // --------------------------------------------------------------- probes

    async fn probe_sessions(&mut self, now: DateTime<Utc>) -> Result<()> {
        for mut session in self.store.list_live_sessions()? {
            let Some(mut task) = self.store.get_task(session.task_id)? else {
                session.state = SessionState::Killed;
                session.ended_at = Some(now);
                self.store.update_session(&session)?;
                self.forget_session(session.id);
                continue;
            };
            let probe = match probe_session(&self.rt.tmux, &session) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(task = %task.key, error = %format!("{e:#}"), "probe failed");
                    continue;
                }
            };
            let sc = &self.cfg.scheduler;
            let silent = now - session.last_activity_at;
            let stale = matches!(task.state, TaskState::Running | TaskState::Starting)
                && sc.stale_session_secs > 0
                && silent > Duration::seconds(sc.stale_session_secs as i64);
            let timeout = sc.max_session_secs > 0 && now - session.started_at > Duration::seconds(sc.max_session_secs as i64);
            let dying = !probe.is_alive() && !(task.state.is_terminal() || task.state == TaskState::Paused);
            let pane_tail = if dying || stale || timeout {
                session.pane_id.as_deref().and_then(|p| self.rt.tmux.capture_pane(p, CRASH_TAIL_LINES).ok())
            } else {
                None
            };
            let ctx = ProbeContext { now, nudged: self.rt.nudged.contains(&session.id), pane_tail };
            let effects = transitions::on_probe(&mut task, &mut session, &probe, &self.cfg.scheduler, &ctx);
            if effects.is_empty() {
                continue;
            }
            self.store.update_session(&session)?;
            self.store.update_task(&task)?;
            self.apply_effects(&mut task, Some(&session), effects).await;
            self.store.update_task(&task)?;
            if !session.state.is_live() {
                self.forget_session(session.id);
            }
        }
        Ok(())
    }

    /// Tasks finished outside the hook path (`powerqueue task complete`,
    /// cancelled by Linear sync, ...) still have a live session: release it.
    async fn finalize_terminal(&mut self, now: DateTime<Utc>) -> Result<()> {
        for mut session in self.store.list_live_sessions()? {
            let Some(mut task) = self.store.get_task(session.task_id)? else { continue };
            if !task.state.is_terminal() {
                continue;
            }
            session.state = SessionState::Exited;
            session.ended_at = Some(now);
            self.store.update_session(&session)?;
            let succeeded = task.state == TaskState::Completed;
            let mut effects = vec![
                Effect::Log {
                    level: EventLevel::Info,
                    kind: "session.finalized".into(),
                    message: format!("task is {}; releasing its session", task.state),
                    data: serde_json::json!({}),
                },
                Effect::Cleanup { succeeded },
            ];
            match task.state {
                TaskState::Completed => effects.push(Effect::Linear {
                    target: LinearTarget::Done,
                    comment: Some(format!(
                        "powerqueue completed this task on branch `{}`.\n\n{}",
                        task.branch.as_deref().unwrap_or("?"),
                        task.summary.as_deref().unwrap_or("No summary was provided.")
                    )),
                }),
                TaskState::Failed => effects.push(Effect::Linear {
                    target: LinearTarget::Blocked,
                    comment: Some(format!(
                        "powerqueue failed this task: {}",
                        task.last_error.as_deref().unwrap_or("unknown error")
                    )),
                }),
                _ => {}
            }
            self.apply_effects(&mut task, Some(&session), effects).await;
            self.store.update_task(&task)?;
            self.forget_session(session.id);
        }
        Ok(())
    }

    // ------------------------------------------------------------ resources

    fn sample_resources(&mut self, now: DateTime<Utc>) -> Result<()> {
        let interval = Duration::seconds(self.cfg.scheduler.resource_sample_secs.max(1) as i64);
        if self.rt.last_resource_sample.is_none_or(|t| now - t >= interval) {
            self.rt.last_resource_sample = Some(now);
            for session in self.store.list_live_sessions()? {
                if let Some(sample) = sample_resources(&mut self.rt.system, &session) {
                    self.store.record_resource_sample(&sample)?;
                }
            }
        }
        if self.rt.last_prune.is_none_or(|t| now - t >= Duration::hours(1)) {
            self.rt.last_prune = Some(now);
            let removed = self.store.prune_resource_samples(now - RESOURCE_RETENTION)?;
            if removed > 0 {
                tracing::debug!(removed, "pruned old resource samples");
            }
        }
        Ok(())
    }

    // --------------------------------------------------------------- launch

    async fn launch_tasks(&mut self, now: DateTime<Utc>) -> Result<()> {
        let live = self.store.list_live_sessions()?;
        let mut slots = self.cfg.scheduler.max_concurrent.saturating_sub(live.len() as u32);
        if slots == 0 {
            return Ok(());
        }
        let busy: HashSet<TaskId> = live.iter().map(|s| s.task_id).collect();
        let tasks: Vec<Task> = self.store.list_open_tasks()?.into_iter().filter(|t| !busy.contains(&t.id)).collect();
        if !tasks.iter().any(|t| t.state.is_schedulable()) {
            return Ok(());
        }
        let estimator = Estimator::from_summaries(&self.store.task_usage_summaries()?);
        let clock = self.clock(now);
        let mut ledger = Ledger::load(&self.store, &self.cfg.budget, &clock, now)?;
        self.rt.rate_limits.clear_expired(now);

        let mut considered: HashSet<TaskId> = HashSet::new();
        while slots > 0 {
            let candidates: Vec<Task> = tasks.iter().filter(|t| !considered.contains(&t.id)).cloned().collect();
            let Some(task) = pick_next(&candidates, now).cloned() else { break };
            considered.insert(task.id);
            let prediction = estimator.predict(&task);
            // Rules express a *preference* (`## Models`, `KEY: model = x`); only
            // `task model <tier>` on the CLI is a hard override. Either way the
            // policy may still downgrade when the tier is out of budget.
            let rule_model =
                self.rt.rules.evaluate(&task, now, None, 0.0, 0.0).model.or_else(|| self.rt.rules.model_for(task.criticality));
            let preferred = task.model_override.or(rule_model);
            let decision = Policy::new(&self.cfg.budget, &ledger, &self.rt.rate_limits).decide(&task, prediction, preferred);
            match decision.model {
                None => self.throttle(task, &decision, now)?,
                Some(model) => {
                    let cost = decision.prediction.weighted_tokens * tier_weight(&self.cfg.budget, model);
                    let key = task.key.clone();
                    let id = task.id;
                    match self.start_task(task, model, &decision, now).await {
                        Ok(()) => {
                            slots -= 1;
                            reserve(&mut ledger, model, cost);
                        }
                        Err(e) => {
                            tracing::error!(task = %key, error = %format!("{e:#}"), "start failed");
                            self.log(
                                Some(id),
                                None,
                                EventLevel::Error,
                                "task.error",
                                &format!("start failed: {e:#}"),
                                serde_json::json!({}),
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn throttle(&mut self, mut task: Task, decision: &Decision, now: DateTime<Utc>) -> Result<()> {
        let retry_at = decision.retry_at.unwrap_or(now + crate::budget::WINDOW_RECHECK);
        let was = task.state;
        task.state = TaskState::Throttled;
        task.not_before = Some(retry_at);
        self.store.update_task(&task)?;
        if was != TaskState::Throttled {
            self.log(
                Some(task.id),
                None,
                EventLevel::Info,
                "task.throttled",
                &format!("no model fits the budget; retry at {}", retry_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                serde_json::json!({ "retry_at": retry_at, "reasons": decision.reasons, "prediction": decision.prediction }),
            );
        } else {
            tracing::debug!(task = %task.key, retry_at = %retry_at, "still throttled");
        }
        Ok(())
    }

    /// Create the worktree, run setup, launch Claude and record the session.
    async fn start_task(&mut self, mut task: Task, model: ModelTier, decision: &Decision, now: DateTime<Utc>) -> Result<()> {
        let attempt = task.attempts + 1;
        task.state = TaskState::Starting;
        task.model = Some(model);
        self.store.update_task(&task)?;
        self.log(
            Some(task.id),
            None,
            EventLevel::Info,
            "task.starting",
            &format!("starting attempt {attempt} with {model}"),
            serde_json::json!({ "model": model, "attempt": attempt, "reasons": decision.reasons, "prediction": decision.prediction }),
        );

        if self.cfg.repo.fetch_before_start
            && let Err(e) = self.rt.repo.fetch()
        {
            self.log(
                Some(task.id),
                None,
                EventLevel::Warn,
                "repo.fetch_failed",
                &format!("git fetch failed; continuing: {e:#}"),
                serde_json::json!({}),
            );
        }
        let branch =
            task.branch.clone().unwrap_or_else(|| branch_name(&self.cfg.repo.branch_template, &task.slug(), &task.id.short()));
        let root = self.cfg.worktree_root(&self.paths);
        let worktree = task.worktree_path.clone().map(PathBuf::from).unwrap_or_else(|| worktree_dir(&root, &task));
        task.branch = Some(branch.clone());
        task.worktree_path = Some(worktree.display().to_string());
        task.attempts = attempt;

        if let Err(e) = self.prepare_worktree(&task, &root, &worktree, &branch) {
            let effects = transitions::on_crash(
                &mut task,
                None,
                &format!("worktree setup failed: {e:#}"),
                None,
                &self.cfg.scheduler,
                now,
                None,
            );
            self.store.update_task(&task)?;
            self.apply_effects(&mut task, None, effects).await;
            self.store.update_task(&task)?;
            return Ok(());
        }

        let previous = self.store.latest_session(task.id)?;
        let (session_id, resume) = match &previous {
            Some(p) if p.state == SessionState::Crashed => (p.id, true),
            _ => (uuid::Uuid::new_v4(), false),
        };
        let launched = self
            .rt
            .launcher
            .prepare(&self.cfg, &task, session_id, model, attempt, resume, task.last_error.as_deref())
            .and_then(|plan| self.rt.launcher.launch(&self.cfg, &task, &plan, session_id, model, attempt));
        let session = match launched {
            Ok(s) => s,
            Err(e) => {
                let effects = transitions::on_crash(
                    &mut task,
                    None,
                    &format!("launch failed: {e:#}"),
                    None,
                    &self.cfg.scheduler,
                    now,
                    None,
                );
                self.store.update_task(&task)?;
                self.apply_effects(&mut task, None, effects).await;
                self.store.update_task(&task)?;
                return Ok(());
            }
        };
        if resume && previous.as_ref().is_some_and(|p| p.id == session.id) {
            self.store.update_session(&session)?;
        } else {
            self.store.insert_session(&session)?;
        }
        self.forget_session(session.id);

        task.state = TaskState::Running;
        task.not_before = None;
        task.started_at = task.started_at.or(Some(now));
        self.store.update_task(&task)?;
        self.log(
            Some(task.id),
            Some(session.id),
            EventLevel::Info,
            "session.launched",
            &format!(
                "launched {model} session (attempt {attempt}{}) in window {}",
                if resume { ", resumed" } else { "" },
                session.tmux_window
            ),
            serde_json::json!({
                "model": model,
                "attempt": attempt,
                "resume": resume,
                "branch": branch,
                "worktree": worktree,
                "window": session.tmux_window,
                "pane": session.pane_id,
            }),
        );
        let comment = format!("powerqueue started attempt {attempt} on branch `{branch}` with model {model}.");
        self.update_linear(&task, LinearTarget::InProgress, Some(comment)).await;
        Ok(())
    }

    fn prepare_worktree(&self, task: &Task, root: &Path, worktree: &Path, branch: &str) -> Result<()> {
        std::fs::create_dir_all(root).with_context(|| format!("create worktree root {}", root.display()))?;
        let base = match &self.cfg.repo.default_branch {
            Some(b) => b.clone(),
            None => self.rt.repo.default_branch().context("detect the default branch (set repo.default_branch)")?,
        };
        self.rt
            .repo
            .add_worktree(worktree, branch, &base)
            .with_context(|| format!("create worktree {} on {branch}", worktree.display()))?;
        if !self.cfg.repo.setup.is_empty() {
            let env = vec![
                ("POWERQUEUE_TASK_ID".to_string(), task.id.to_string()),
                ("POWERQUEUE_TASK_KEY".to_string(), task.key.clone()),
                ("POWERQUEUE_BRANCH".to_string(), branch.to_string()),
                ("POWERQUEUE_WORKTREE".to_string(), worktree.display().to_string()),
            ];
            let output = run_commands(worktree, &self.cfg.repo.setup, &env).context("repo.setup commands")?;
            self.log(Some(task.id), None, EventLevel::Debug, "worktree.setup", "setup commands finished", serde_json::json!({ "output": output.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect::<String>() }));
        }
        self.log(
            Some(task.id),
            None,
            EventLevel::Info,
            "worktree.ready",
            &format!("worktree {} on {branch}", worktree.display()),
            serde_json::json!({ "path": worktree, "branch": branch, "base": base }),
        );
        Ok(())
    }
}

/// Count a predicted cost against the in-memory ledger so several launches in
/// one tick do not each think they are the only one.
fn reserve(ledger: &mut Ledger, tier: ModelTier, weighted_cost: f64) {
    if let Some(t) = ledger.tiers.iter_mut().find(|t| t.tier == tier) {
        t.period_weighted += weighted_cost;
        t.window_weighted += weighted_cost;
    }
    ledger.total_period_weighted += weighted_cost;
    ledger.total_window_weighted += weighted_cost;
}

/// Resolve on SIGINT (ctrl-c) or, on unix, SIGTERM.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).ok();
        let term_fut = async {
            match term.as_mut() {
                Some(t) => {
                    t.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term_fut => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{Period, TierLedger};

    #[test]
    fn reserve_counts_against_tier_and_totals() {
        let now = Utc::now();
        let mut ledger = Ledger {
            now,
            period: Period { start: now, end: now + Duration::days(7) },
            window: Period { start: now - Duration::hours(5), end: now },
            tiers: vec![TierLedger { tier: ModelTier::Opus, period_budget: 100.0, ..Default::default() }],
            total_period_weighted: 0.0,
            total_window_weighted: 0.0,
            period_budget: 100.0,
            window_budget: 50.0,
            calibration: None,
        };
        reserve(&mut ledger, ModelTier::Opus, 30.0);
        reserve(&mut ledger, ModelTier::Haiku, 5.0);
        assert_eq!(ledger.tier(ModelTier::Opus).period_weighted, 30.0);
        assert_eq!(ledger.tier(ModelTier::Opus).window_weighted, 30.0);
        assert_eq!(ledger.total_period_weighted, 35.0);
        assert_eq!(ledger.total_window_weighted, 35.0);
    }

    #[test]
    fn handle_stop_flag() {
        let h = DaemonHandle::new();
        assert!(!h.should_stop());
        h.stop();
        assert!(h.should_stop());
    }
}
