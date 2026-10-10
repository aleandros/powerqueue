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

use crate::budget::{
    Decision, Estimator, Ledgers, PeriodClock, RATE_LIMITS_KEY, RateLimitState, load_observed, probe_all, tier_weight,
};
use crate::config::{Config, OverridesSource, REPO_CONFIG_FILE, RulesSource};
use crate::domain::{
    BRANCH_CREATED_EVENT, DaemonCommand, EventLevel, ModelTier, Provider, SCHEDULING_PAUSE_KEY, SchedulingPause, Session,
    SessionState, Task, TaskId, TaskSource, TaskState, is_closed_state_type,
};
use crate::github::{Gh, GitHubClient, PrRef, apply_plan, plan_sync};
use crate::jev::{JevClient, JevQuestion, content_hash};
use crate::linear::{
    IssueFilter, LinearClient, LinearIssue, WATCHED_PARENTS_KEY, WatchedParents, container_comment, container_finished,
    parents_to_watch, sync_issues,
};
use crate::paths::Paths;
use crate::priority::{PriorityRules, RulesWatcher};
use crate::secrets::{SecretKind, Secrets};
use crate::session::{
    Launcher, TranscriptReader, agent_for, interpret_hook, probe_session, sample_resources, transcript::claude_home,
    transcript_path_for,
};
use crate::store::{JevCached, PendingHookEvent, Store};
use crate::tmux::Tmux;
use crate::worktree::{BasePreference, FastForward, Repo, run_commands};

use super::commands;
use super::launch::{self, Candidate, LaunchContext, LaunchPlanner};
use super::lifecycle::cleanup_task;
use super::relay::{self, PostedQuestion, RelayState, relay_key};
use super::review;
pub use super::transitions::SKIP_REASON;
use super::transitions::{self, CRASH_TAIL_LINES, Effect, LinearTarget, ProbeContext};
/// Longest pause between Linear polls after repeated failures.
const LINEAR_MAX_BACKOFF: Duration = Duration::minutes(10);
/// How often `.powerqueue.toml` (and committed rules) are fingerprinted for
/// an automatic reload.
const OVERRIDES_CHECK_SECS: i64 = 10;
/// How long a tick waits for the periodic `git fetch` before letting it
/// finish in the background.
const FETCH_GRACE_SECS: u64 = 2;

/// Log the outcome of a periodic fetch run on a blocking thread.
fn report_fetch(result: std::result::Result<Result<()>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => tracing::debug!("periodic fetch done"),
        Ok(Err(e)) => tracing::warn!(error = %format!("{e:#}"), "periodic fetch failed; repository overrides stay as last read"),
        Err(e) => tracing::warn!(error = %e, "periodic fetch panicked; repository overrides stay as last read"),
    }
}
/// How long resource samples are kept.
const RESOURCE_RETENTION: Duration = Duration::days(7);
/// Ended sessions keep having their transcript tailed for this long.
const TRANSCRIPT_GRACE: Duration = Duration::minutes(5);
/// Watched parent issues are looked up at most this often (or on a forced
/// sync), so a parent that never finishes does not cost a request per poll.
const PARENT_CHECK_INTERVAL: Duration = Duration::minutes(5);
/// A blocker whose PR was not merged is asked again after this long.
const MERGE_RECHECK: Duration = Duration::minutes(5);

/// Which `linear.post_comments` setting a comment needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentKind {
    /// Routine progress (started, completed, failed): only with `true`.
    Progress,
    /// Questions and notices (merge/hold, review rounds): also with `"questions"`.
    Notice,
}

/// What a relayed comment is for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayMode {
    /// The reply to an open question.
    Answer,
    /// A hint for a session that is working.
    Hint,
}

/// How [`Daemon::send_answer`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// Typed into the live session.
    Sent,
    /// A live session exists but could not be typed into.
    Failed,
    /// No live session to type into.
    NoSession,
}

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

#[derive(Default)]
struct GitHubRuntime {
    client: Option<GitHubClient>,
    warned: bool,
    last_poll: Option<DateTime<Utc>>,
    backoff: Duration,
    next_allowed: Option<DateTime<Utc>>,
}

/// See [`Daemon::refresh_overrides`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct OverridesFingerprint {
    /// `.powerqueue.toml`: mtime and size, or `<rev>:<blob>`; `None` = absent.
    overrides: Option<String>,
    /// The committed rules blob, when the rules come from the branch.
    rules: Option<String>,
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
    github: GitHubRuntime,
    linear: Option<LinearClient>,
    linear_warned: bool,
    last_linear_poll: Option<DateTime<Utc>>,
    linear_backoff: Duration,
    linear_next_allowed: Option<DateTime<Utc>>,
    force_sync: bool,
    last_parent_check: Option<DateTime<Utc>>,
    /// When issue comments were last read for the question relay.
    last_relay_poll: Option<DateTime<Utc>>,
    /// Blockers known to have a merged PR (merged stays merged).
    merged_blockers: HashSet<String>,
    /// When each blocker was last found *not* merged.
    unmerged_checked: HashMap<String, DateTime<Utc>>,
    jev: Option<JevClient>,
    jev_error_logged: bool,
    last_resource_sample: Option<DateTime<Utc>>,
    last_prune: Option<DateTime<Utc>>,
    /// What `.powerqueue.toml` (and committed rules) looked like at the last
    /// check; a change reloads without a `Reload` command.
    overrides_fingerprint: Option<OverridesFingerprint>,
    last_overrides_check: Option<DateTime<Utc>>,
    /// Last periodic `git fetch` (`repo.fetch_interval_secs`); a fetch
    /// before a task start counts too.
    last_fetch: Option<DateTime<Utc>>,
    /// The periodic fetch running on a blocking thread, so an unreachable
    /// remote never stalls the tick; its result is read on the next check.
    periodic_fetch: Option<tokio::task::JoinHandle<Result<()>>>,
    /// A git failure while fingerprinting is reported once, not every check.
    fingerprint_error_logged: bool,
    /// The fingerprint whose reload failed and was reported; the reload is
    /// retried silently every check until it succeeds or the file changes.
    reload_failed_for: Option<OverridesFingerprint>,
    /// Usage probes running on a blocking thread (they record their own results).
    usage_probe: Option<tokio::task::JoinHandle<()>>,
    last_usage_probe: Option<DateTime<Utc>>,
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
            github: GitHubRuntime::default(),
            linear: None,
            linear_warned: false,
            last_linear_poll: None,
            linear_backoff: Duration::zero(),
            linear_next_allowed: None,
            force_sync: false,
            last_parent_check: None,
            last_relay_poll: None,
            merged_blockers: HashSet::new(),
            unmerged_checked: HashMap::new(),
            jev: None,
            jev_error_logged: false,
            last_resource_sample: None,
            last_prune: None,
            overrides_fingerprint: None,
            last_overrides_check: None,
            last_fetch: None,
            periodic_fetch: None,
            fingerprint_error_logged: false,
            reload_failed_for: None,
            usage_probe: None,
            last_usage_probe: None,
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
        // Issue sources first, rules second: a ticket created this tick gets its
        // criticality, score and preferred model before `launch_tasks` sees
        // it (otherwise an urgent ticket is first scheduled as `normal`).
        let r = self.refresh_overrides(now).await;
        self.report_phase("config", r);
        let force_sync = self.rt.force_sync;
        let r = self.poll_linear(now).await;
        self.report_phase("linear", r);
        // Each source gets the request once; Linear also consumes it when
        // polled directly (including its throttled parent checks).
        self.rt.force_sync = force_sync;
        let r = self.poll_github(now).await;
        self.report_phase("github", r);
        self.rt.force_sync = false;
        let r = self.refresh_rules(now).await;
        self.report_phase("rules", r);
        // Inbox before hooks: a shim-delivered hook becomes a hook row that
        // the hook phase handles on this same tick.
        let r = self.process_inbox(now).await;
        self.report_phase("inbox", r);
        let r = self.process_hooks(now).await;
        self.report_phase("hooks", r);
        let r = self.relay_comments(now).await;
        self.report_phase("relay", r);
        let r = self.tail_transcripts(now).await;
        self.report_phase("transcripts", r);
        let r = self.probe_sessions(now).await;
        self.report_phase("probes", r);
        let r = self.finalize_terminal(now).await;
        self.report_phase("finalize", r);
        let r = self.watch_reviews(now).await;
        self.report_phase("reviews", r);
        let r = self.sample_resources(now);
        self.report_phase("resources", r);
        self.run_usage_probes(now);
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

    /// End of `provider`'s current period (an observed reset in kv overrides
    /// the configured anchor, as in [`crate::budget::Ledger::load`]).
    /// Rate-limit cooldowns are capped there: the allowance resets anyway.
    fn provider_period_end(&self, provider: Provider, now: DateTime<Utc>) -> DateTime<Utc> {
        let mut clock = PeriodClock::from_provider(self.cfg.budget.provider(provider), now);
        if let Ok(Some(reset)) = load_observed(&self.store, provider).map(|o| o.and_then(|o| o.period_resets_at))
            && reset > now
        {
            clock = clock.with_observed_anchor(reset);
        }
        clock.current_period(now).end
    }

    /// Until when `provider` is cooling down, if it is: every enabled model
    /// rate-limited, or a stored observation saying the allowance is
    /// exhausted ([`crate::budget::ObservedUsage::cooldown_until`]). The
    /// later of the two wins.
    fn provider_cooldown(&self, provider: Provider, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let marks = self.rt.rate_limits.provider_blocked_until(&self.cfg.budget, provider, now);
        let observed = load_observed(&self.store, provider).ok().flatten().and_then(|o| o.cooldown_until()).filter(|u| *u > now);
        match (marks, observed) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// Put `provider` on cooldown for its `rate_limit_cooldown_mins` (capped
    /// at its period end), persist it and log `budget.rate_limited`. Used
    /// when a pane shows the agent waiting for a usage reset that no hook
    /// reported. Returns the cooldown end.
    fn start_provider_cooldown(&mut self, provider: Provider, task: &Task, reason: &str, now: DateTime<Utc>) -> DateTime<Utc> {
        let mins = self.cfg.budget.provider(provider).rate_limit_cooldown_mins.max(1) as i64;
        let until = (now + Duration::minutes(mins)).min(self.provider_period_end(provider, now)).max(now + Duration::minutes(1));
        self.rt.rate_limits.mark_provider(&self.cfg.budget, provider, until);
        if let Err(e) = self.store.kv_set(RATE_LIMITS_KEY, &self.rt.rate_limits) {
            tracing::warn!(error = %e, "cannot persist rate limits");
        }
        self.log(
            Some(task.id),
            None,
            EventLevel::Warn,
            "budget.rate_limited",
            &format!(
                "{provider}: {reason}; cooling down every {provider} model until {}",
                until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
            serde_json::json!({ "provider": provider, "until": until, "account_wide": true, "source": "pane" }),
        );
        until
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
                let live = self.store.latest_session(task.id)?.is_some_and(|s| s.state.is_live());
                let before = task.clone();
                let effects = commands::on_pause(&mut task, live);
                self.commit(&before, &mut task, None, effects).await?;
            }
            DaemonCommand::Resume { task_id } => {
                let mut task = self.require_task(*task_id)?;
                let live = self.store.latest_session(task.id)?.filter(|s| s.state.is_live());
                let before = task.clone();
                let effects = commands::on_resume(&mut task, live.is_some(), now);
                self.commit(&before, &mut task, live.as_ref(), effects).await?;
            }
            DaemonCommand::Cancel { task_id } => {
                let mut task = self.require_task(*task_id)?;
                let mut session = self.store.latest_session(task.id)?.filter(|s| s.state.is_live());
                let before = (task.clone(), session.clone());
                let effects = commands::on_cancel(&mut task, session.as_mut(), now);
                let ended = session.as_ref().filter(|s| Some(*s) != before.1.as_ref());
                if let Some(session) = ended {
                    self.store.update_session(session)?;
                }
                self.commit(&before.0, &mut task, ended, effects).await?;
            }
            DaemonCommand::Retry { task_id } => {
                let mut task = self.require_task(*task_id)?;
                let mut session = self.store.latest_session(task.id)?.filter(|s| s.state.is_live());
                let before = (task.clone(), session.clone());
                let effects = commands::on_retry(&mut task, session.as_mut(), now);
                // The events name the session only when the retry ended it.
                let ended = session.as_ref().filter(|s| Some(*s) != before.1.as_ref());
                if let Some(session) = ended {
                    self.store.update_session(session)?;
                }
                self.commit(&before.0, &mut task, ended, effects).await?;
            }
            DaemonCommand::SetModel { task_id, model } => {
                let mut task = self.require_task(*task_id)?;
                let before = task.clone();
                let effects = commands::on_set_model(&mut task, model.clone());
                self.commit(&before, &mut task, None, effects).await?;
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
            DaemonCommand::PauseScheduling { reason } => {
                // The CLI writes the kv row itself (so the pause holds at
                // once); the command is the daemon's cue to log it. A
                // command without the row (an older CLI) writes it here.
                if self.store.scheduling_pause()?.is_none() {
                    let pause = SchedulingPause { since: now, reason: reason.clone() };
                    self.store.kv_set(SCHEDULING_PAUSE_KEY, &pause)?;
                }
                let live = self.store.list_live_sessions()?.len();
                self.log(
                    None,
                    None,
                    EventLevel::Info,
                    "daemon.paused",
                    &format!("scheduling paused: no new session until `powerqueue resume` ({live} running session(s) continue)"),
                    serde_json::json!({ "reason": reason, "live_sessions": live }),
                );
            }
            DaemonCommand::ResumeScheduling => {
                self.store.kv_delete(SCHEDULING_PAUSE_KEY)?;
                self.log(None, None, EventLevel::Info, "daemon.resumed", "scheduling resumed", serde_json::json!({}));
            }
        }
        Ok(())
    }

    /// Persist what a pure transition changed on `task` (compared with
    /// `before`), carry out its effects, and persist again when an effect
    /// (cleanup, branch deletion) changed the task further.
    async fn commit(&mut self, before: &Task, task: &mut Task, session: Option<&Session>, effects: Vec<Effect>) -> Result<()> {
        if *task != *before {
            self.store.update_task(task)?;
        }
        if effects.is_empty() {
            return Ok(());
        }
        let saved = task.clone();
        self.apply_effects(task, session, effects).await;
        if *task != saved {
            self.store.update_task(task)?;
        }
        Ok(())
    }

    /// Log the events of a transition that requests nothing else (any other
    /// effect is a programming error and is reported at warn level).
    fn log_only(&self, task: &Task, session: Option<&Session>, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Log { level, kind, message, data } => {
                    self.log(Some(task.id), session.map(|s| s.id), level, &kind, &message, data)
                }
                other => tracing::warn!(task = %task.key, effect = ?other, "effect ignored in a log-only phase"),
            }
        }
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
        self.rt.github = GitHubRuntime::default();
        self.rt.linear = None;
        self.rt.linear_warned = false;
        self.rt.jev = None;
        self.rt.rules_loaded = false;
        self.rt.watcher = None;
        self.rt.watcher_failed = false;
        self.cfg = cfg;
        // What was just loaded is the new baseline, whichever way the reload
        // was requested; `refresh_overrides` must not apply it a second time.
        self.rt.overrides_fingerprint = match self.overrides_fingerprint() {
            Ok(fp) => Some(fp),
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "cannot fingerprint the overrides after a reload");
                None
            }
        };
        self.rt.reload_failed_for = None;
        Ok(())
    }

    // --------------------------------------------------------------- rules

    async fn refresh_rules(&mut self, now: DateTime<Utc>) -> Result<()> {
        let source = self.cfg.rules_source(&self.paths);
        // Rules on disk are watched; committed rules are polled through git
        // by `refresh_overrides`, which clears `rules_loaded` on a change.
        if let RulesSource::File(path) = &source
            && self.rt.watcher.is_none()
            && !self.rt.watcher_failed
            && self.cfg.priority.live_reload
        {
            match RulesWatcher::new(path) {
                Ok(w) => self.rt.watcher = Some(w),
                Err(e) => {
                    self.rt.watcher_failed = true;
                    tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "cannot watch PRIORITY.md; use `powerqueue run` restart or Reload to pick up changes");
                }
            }
        }
        let changed = !self.rt.rules_loaded || self.rt.watcher.as_mut().is_some_and(|w| w.take_changed());
        if changed {
            self.load_rules(&source);
        }

        let tasks = self.store.list_open_tasks()?;
        for mut task in tasks {
            // Never under a live session: a throttled or paused task may
            // keep one (the agent waits in-session for its usage reset, or
            // finishes its turn), and a re-score could block or re-queue
            // the task under it, leaving the window unattended.
            if task.state.has_live_session()
                || (task.state.may_keep_session() && self.store.latest_session(task.id)?.is_some_and(|s| s.state.is_live()))
            {
                continue;
            }
            let jev_norm = self.jev_norm_for(&task, now).await;
            let eval =
                self.rt.rules.evaluate(&task, now, jev_norm, self.cfg.priority.jev.weight, self.cfg.priority.age_boost_per_hour);
            let before = task.clone();
            let effects = transitions::on_evaluation(&mut task, &eval);
            self.log_only(&task, None, effects);
            if task != before {
                self.store.update_task(&task)?;
                tracing::debug!(task = %task.key, criticality = %task.criticality, score = task.score, "re-scored");
            }
        }
        Ok(())
    }

    fn load_rules(&mut self, source: &RulesSource) {
        self.rt.rules_loaded = true;
        let shown = source.describe();
        let path = source.path();
        let text = match source.read() {
            Ok(Some(t)) => t,
            Ok(None) => {
                if !self.rt.rules_missing_logged {
                    self.rt.rules_missing_logged = true;
                    self.log(
                        None,
                        None,
                        EventLevel::Warn,
                        "rules.missing",
                        &format!("{shown} does not exist; using default rules"),
                        serde_json::json!({ "path": path, "rev": source.rev() }),
                    );
                }
                self.rt.rules = PriorityRules::default();
                return;
            }
            Err(e) => {
                // Unreadable (permissions, git failure): keep what we have
                // rather than silently dropping every rule.
                self.log(
                    None,
                    None,
                    EventLevel::Warn,
                    "rules.unreadable",
                    &format!("cannot read {shown} ({e:#}); keeping the previous rules"),
                    serde_json::json!({ "path": path, "rev": source.rev(), "error": format!("{e:#}") }),
                );
                return;
            }
        };
        self.rt.rules_missing_logged = false;
        match PriorityRules::parse(&text) {
            Ok(rules) => {
                let warnings: Vec<String> = rules.warnings.iter().map(|w| w.to_string()).collect();
                self.log(
                    None,
                    None,
                    EventLevel::Info,
                    "rules.loaded",
                    &format!("loaded {shown} ({} warning(s))", warnings.len()),
                    serde_json::json!({ "path": path, "rev": source.rev(), "warnings": warnings }),
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
                    &format!("{shown} has errors; keeping the previous rules: {}", errors.join("; ")),
                    serde_json::json!({ "path": path, "rev": source.rev(), "errors": errors }),
                );
            }
        }
    }

    // ----------------------------------------------------------- overrides

    /// Pick up a changed `.powerqueue.toml` without a `Reload` command. Every
    /// [`OVERRIDES_CHECK_SECS`] the file is fingerprinted (working tree:
    /// mtime and size; default branch: the blob ids of the file and of the
    /// committed rules, after a periodic `git fetch` every
    /// `repo.fetch_interval_secs`, run on a blocking thread so an
    /// unreachable remote never stalls the tick). A change reloads the
    /// configuration (event `daemon.reloaded`), or only the rules when
    /// nothing but the committed rules changed. A file that does not load
    /// keeps the previous configuration (event `daemon.reload_failed`, once
    /// per change) and the reload is retried every check, so a transient
    /// failure (a git lock, a half-written `config.toml`) heals on its own.
    /// A git failure while fingerprinting is logged once and the last good
    /// fingerprint kept, so it never counts as a change.
    async fn refresh_overrides(&mut self, now: DateTime<Utc>) -> Result<()> {
        if self.rt.last_overrides_check.is_some_and(|t| now - t < Duration::seconds(OVERRIDES_CHECK_SECS)) {
            return Ok(());
        }
        self.rt.last_overrides_check = Some(now);
        if !self.cfg.repo_path().exists() {
            return Ok(());
        }
        // The baseline is what the configuration was loaded from, taken
        // *before* the first fetch: a change merged while the daemon was
        // down must count as a change, not as the starting point.
        if self.rt.overrides_fingerprint.is_none() {
            match self.overrides_fingerprint() {
                Ok(fp) => self.rt.overrides_fingerprint = Some(fp),
                Err(e) => {
                    self.fingerprint_error_once(&e);
                    return Ok(());
                }
            }
        }
        if let Some(handle) = self.rt.periodic_fetch.take() {
            // A fetch started on an earlier check: compare after it landed,
            // never against half-updated refs.
            if !handle.is_finished() {
                self.rt.periodic_fetch = Some(handle);
                return Ok(());
            }
            report_fetch(handle.await);
        }
        let from_branch = self.cfg.repo.overrides_from == OverridesSource::DefaultBranch;
        if from_branch && self.cfg.repo.fetch_interval_secs > 0 && !self.offline {
            let interval =
                Duration::try_seconds(self.cfg.repo.fetch_interval_secs.min(i64::MAX as u64) as i64).unwrap_or(Duration::MAX);
            if self.rt.last_fetch.is_none_or(|t| now - t >= interval) {
                self.rt.last_fetch = Some(now);
                let repo = self.rt.repo.clone();
                let mut handle = tokio::task::spawn_blocking(move || repo.fetch());
                // A reachable remote answers within the grace period and the
                // comparison happens right away; a hanging one keeps running
                // on its thread while the tick goes on.
                match tokio::time::timeout(std::time::Duration::from_secs(FETCH_GRACE_SECS), &mut handle).await {
                    Ok(result) => report_fetch(result),
                    Err(_) => {
                        tracing::debug!("periodic fetch still running; checking overrides after it finishes");
                        self.rt.periodic_fetch = Some(handle);
                        return Ok(());
                    }
                }
            }
        }
        let current = match self.overrides_fingerprint() {
            Ok(fp) => fp,
            Err(e) => {
                self.fingerprint_error_once(&e);
                return Ok(());
            }
        };
        self.rt.fingerprint_error_logged = false;
        let Some(previous) = self.rt.overrides_fingerprint.clone() else {
            return Ok(());
        };
        if previous == current {
            return Ok(());
        }
        let file = self.cfg.repo_path().join(REPO_CONFIG_FILE);
        if previous.overrides == current.overrides {
            // Only the committed rules moved: no need to rebuild clients.
            self.rt.overrides_fingerprint = Some(current);
            self.rt.rules_loaded = false;
            return Ok(());
        }
        match self.reload_config() {
            Ok(()) => {
                let ov = &self.cfg.overrides;
                let keys = if ov.keys.is_empty() { "no keys".to_string() } else { ov.keys.join(", ") };
                self.log(
                    None,
                    None,
                    EventLevel::Info,
                    "daemon.reloaded",
                    &format!("{} changed ({}); configuration and rules reloaded: {keys}", file.display(), ov.origin()),
                    serde_json::json!({ "reason": "repo_overrides", "file": file, "rev": ov.rev, "keys": ov.keys }),
                );
            }
            Err(e) if self.rt.reload_failed_for.as_ref() == Some(&current) => {
                // Already reported for this very change; keep retrying quietly.
                tracing::debug!(error = %format!("{e:#}"), "reload still failing; previous configuration kept");
            }
            Err(e) => {
                self.rt.reload_failed_for = Some(current);
                self.log(
                    None,
                    None,
                    EventLevel::Warn,
                    "daemon.reload_failed",
                    &format!("{} changed but cannot be applied; keeping the previous configuration: {e:#}", file.display()),
                    serde_json::json!({ "reason": "repo_overrides", "file": file, "error": format!("{e:#}") }),
                );
            }
        }
        Ok(())
    }

    fn fingerprint_error_once(&mut self, e: &anyhow::Error) {
        if self.rt.fingerprint_error_logged {
            tracing::debug!(error = %format!("{e:#}"), "overrides fingerprint still failing");
            return;
        }
        self.rt.fingerprint_error_logged = true;
        self.log(
            None,
            None,
            EventLevel::Warn,
            "daemon.overrides_check_failed",
            &format!("cannot check {} for changes; keeping the last known state: {e:#}", REPO_CONFIG_FILE),
            serde_json::json!({ "error": format!("{e:#}") }),
        );
    }

    /// What `.powerqueue.toml` and the committed rules currently look like.
    /// Fails when git cannot read the branch; the caller keeps the previous
    /// fingerprint then, so a transient failure is neither a change nor a
    /// reason to reload.
    fn overrides_fingerprint(&self) -> Result<OverridesFingerprint> {
        let repo = self.cfg.repo_path();
        match self.cfg.repo.overrides_from {
            OverridesSource::WorkingTree => {
                let overrides =
                    std::fs::metadata(repo.join(REPO_CONFIG_FILE)).ok().map(|m| format!("{:?}:{}", m.modified().ok(), m.len()));
                Ok(OverridesFingerprint { overrides, rules: None })
            }
            OverridesSource::DefaultBranch => {
                let rules_path = self.cfg.overrides.priority_file_in_repo.clone();
                // The commit the loader read (recorded even when the file was
                // absent): the merged config's `default_branch` may be the
                // repo's own override and must not move the watch.
                let rev = self.cfg.overrides_rev(&self.rt.repo)?;
                let mut paths = vec![REPO_CONFIG_FILE];
                if let Some(p) = &rules_path {
                    paths.push(p);
                }
                let ids = self.rt.repo.blob_ids(&rev, &paths)?;
                Ok(OverridesFingerprint {
                    overrides: ids.get(REPO_CONFIG_FILE).map(|sha| format!("{rev}:{sha}")),
                    rules: rules_path.and_then(|p| ids.get(&p).cloned()),
                })
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
        let forced = self.rt.force_sync;
        self.rt.force_sync = false;
        self.rt.last_linear_poll = Some(now);

        let filter = IssueFilter::from_config(&self.cfg.linear);
        let mut issues = match client.fetch_issues(&filter).await {
            Ok(issues) => issues,
            Err(e) => {
                self.linear_failure(now, &format!("fetching issues failed: {e:#}"));
                return Ok(());
            }
        };

        // `sync_issues` takes a synchronous state lookup, so fetch the states
        // of open tasks whose issues are no longer in the queued set up front.
        // Those still open in Linear are synced too, so their dependencies
        // stay current while they sit outside the queued states.
        let present: HashSet<String> = issues.iter().map(|i| i.id.clone()).collect();
        let mut states: HashMap<String, String> = HashMap::new();
        let open = self.store.list_open_tasks()?;
        let mut still_open: Vec<LinearIssue> = Vec::new();
        for task in open.iter().filter(|t| {
            matches!(
                t.state,
                TaskState::Queued | TaskState::Throttled | TaskState::Paused | TaskState::Crashed | TaskState::Blocked
            )
        }) {
            let Some(issue_id) = task.linear_issue_id() else { continue };
            if present.contains(issue_id) {
                continue;
            }
            match client.get_issue(issue_id).await {
                Ok(Some(issue)) => {
                    states.insert(issue_id.to_string(), issue.state_type.clone());
                    if !is_closed_state_type(&issue.state_type) {
                        still_open.push(issue);
                    }
                }
                Ok(None) => {
                    states.insert(issue_id.to_string(), "canceled".to_string());
                }
                Err(e) => tracing::warn!(task = %task.key, error = %format!("{e:#}"), "cannot fetch issue state"),
            }
        }
        issues.extend(still_open);

        // Without complete relations a blocked task could be started early,
        // so a failure here skips the whole sync.
        let ids: Vec<String> = issues.iter().map(|i| i.id.clone()).collect();
        let deps = match client.fetch_dependencies(&ids).await {
            Ok(d) => d,
            Err(e) => {
                self.linear_failure(now, &format!("fetching issue relations failed: {e:#}"));
                return Ok(());
            }
        };
        for issue in &mut issues {
            match deps.get(&issue.id) {
                Some(d) => {
                    issue.blocked_by = d.blocked_by.clone();
                    issue.children = d.children.clone();
                }
                // Not returned (should not happen): keep what is stored.
                None => {
                    if let Some(t) = self.store.get_task_by_linear_issue(&issue.id)? {
                        issue.blocked_by = t.blocked_by;
                        issue.children = t.children;
                    }
                }
            }
        }
        self.mark_merged_blockers(&client, &mut issues, &open, now).await;

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
        let parents_due = forced || self.rt.last_parent_check.is_none_or(|t| now - t >= PARENT_CHECK_INTERVAL);
        if let Err(e) = self.close_finished_parents(&client, &issues, parents_due, now).await {
            self.log(
                None,
                None,
                EventLevel::Warn,
                "linear.parent_error",
                &format!("checking parent issues failed: {e:#}"),
                serde_json::json!({}),
            );
        }
        Ok(())
    }

    /// Watch parent issues (from this poll's sub-issues and containers, plus
    /// the ones remembered in kv) and close each one whose sub-issues are all
    /// done ([`container_finished`]): move it to `linear.done_state_parent`
    /// (when `linear.manage_states` is on), post [`container_comment`] (when
    /// comments are on) and complete its powerqueue task, if it has one.
    /// A parent is forgotten once it is closed, deleted or has no children;
    /// one whose state change fails stays watched and is retried later.
    /// Parents powerqueue closed are remembered and never handled twice.
    /// New parents are recorded on every poll, but the watched ones are only
    /// looked up when `due` ([`PARENT_CHECK_INTERVAL`] or a forced sync).
    /// A completed container that had run before is cleaned up like any
    /// finished task (worktree, branch, window).
    async fn close_finished_parents(
        &mut self,
        client: &LinearClient,
        issues: &[LinearIssue],
        due: bool,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let mut watched: WatchedParents =
            self.store.kv_get(WATCHED_PARENTS_KEY).context("read watched parents")?.unwrap_or_default();
        let before = watched.clone();
        watched.watch(parents_to_watch(issues));
        let containers: Vec<String> = self
            .store
            .list_open_tasks()?
            .iter()
            .filter(|t| t.is_container())
            .filter_map(|t| t.linear_identifier().map(str::to_string))
            .collect();
        watched.watch(containers);
        let to_check = if due { watched.watching.clone() } else { Default::default() };
        if due {
            self.rt.last_parent_check = Some(now);
        }
        for key in to_check {
            let parent = match client.parent_status(&key).await {
                Ok(Some(p)) => p,
                Ok(None) => {
                    watched.watching.remove(&key);
                    continue;
                }
                Err(e) => {
                    tracing::warn!(parent = %key, error = %format!("{e:#}"), "cannot fetch parent issue");
                    continue;
                }
            };
            if is_closed_state_type(&parent.state_type) || parent.children.is_empty() {
                watched.watching.remove(&key);
                continue;
            }
            if !container_finished(&parent.children) {
                continue;
            }
            let task = self.store.get_task_by_linear_issue(&parent.id)?;
            if task.as_ref().is_some_and(|t| t.state.has_live_session()) {
                tracing::debug!(parent = %key, "parent issue has a live session; not closing it");
                continue;
            }
            let state = self
                .cfg
                .linear
                .done_state_parent
                .clone()
                .filter(|s| !s.trim().is_empty())
                .filter(|_| self.cfg.linear.manage_states);
            if let Some(name) = &state
                && let Err(e) = client.set_state_in_team(&parent.id, &parent.identifier, &parent.team_key, name).await
            {
                self.log(
                    task.as_ref().map(|t| t.id),
                    None,
                    EventLevel::Warn,
                    "linear.parent_error",
                    &format!("could not move parent issue {key} to {name}: {e:#}"),
                    serde_json::json!({ "parent": key, "state": name }),
                );
                continue;
            }
            watched.mark_closed(&key);
            let children: Vec<&str> = parent.children.iter().map(|c| c.key.as_str()).collect();
            let message = match &state {
                Some(name) => format!("all sub-issues of {key} are done ({}); moved it to {name}", children.join(", ")),
                None => format!("all sub-issues of {key} are done ({}); state left alone", children.join(", ")),
            };
            self.log(
                task.as_ref().map(|t| t.id),
                None,
                EventLevel::Info,
                "linear.parent_closed",
                &message,
                serde_json::json!({ "parent": key, "children": children, "state": state }),
            );
            if self.cfg.linear.post_comments.questions() {
                let body = relay::tag_own(&container_comment(&parent.children, state.as_deref()));
                let posted = client.post_comment(&parent.id, &body).await;
                // A parent that is a task too must never get this relayed back.
                if let (Ok(Some(c)), Some(t)) = (&posted, task.as_ref()) {
                    self.update_relay(t, |state| state.record_posted(&c.id));
                }
                if let Err(e) = posted {
                    self.log(
                        task.as_ref().map(|t| t.id),
                        None,
                        EventLevel::Warn,
                        "linear.parent_error",
                        &format!("could not comment on parent issue {key}: {e:#}"),
                        serde_json::json!({ "parent": key }),
                    );
                }
            }
            if let Some(mut task) = task {
                let before = task.clone();
                let effects = transitions::on_container_closed(&mut task, &parent.children, now);
                self.commit(&before, &mut task, None, effects).await?;
            }
        }
        if watched != before {
            self.store.kv_set(WATCHED_PARENTS_KEY, &watched)?;
        }
        Ok(())
    }

    /// For every blocker that is still open in Linear, decide whether its
    /// pull request is merged and set `pr_merged`. Merged stays merged: a
    /// blocker known merged (from an earlier poll or the stored tasks in
    /// `open`) is never asked again, one found unmerged is asked again after
    /// [`MERGE_RECHECK`], and a failed lookup leaves it pending without
    /// forgetting an earlier "merged".
    async fn mark_merged_blockers(
        &mut self,
        client: &LinearClient,
        issues: &mut [LinearIssue],
        open: &[Task],
        now: DateTime<Utc>,
    ) {
        for task in open {
            self.rt.merged_blockers.extend(task.blocked_by.iter().filter(|b| b.pr_merged).map(|b| b.key.clone()));
        }
        for issue in issues.iter_mut() {
            for blocker in issue.blocked_by.iter_mut().filter(|b| !b.is_closed()) {
                if self.rt.merged_blockers.contains(&blocker.key) {
                    blocker.pr_merged = true;
                    continue;
                }
                if self.rt.unmerged_checked.get(&blocker.key).is_some_and(|at| now - *at < MERGE_RECHECK) {
                    continue;
                }
                match client.pr_merged(&blocker.key).await {
                    Ok(true) => {
                        self.rt.merged_blockers.insert(blocker.key.clone());
                        self.rt.unmerged_checked.remove(&blocker.key);
                        blocker.pr_merged = true;
                    }
                    Ok(false) => {
                        self.rt.unmerged_checked.insert(blocker.key.clone(), now);
                    }
                    Err(e) => {
                        tracing::warn!(blocker = %blocker.key, error = %format!("{e:#}"), "cannot check the blocker's pull request");
                    }
                }
            }
        }
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

    /// Best-effort Linear update: move the issue (when `linear.manage_states`
    /// is on and a state is configured for `target`) and post `comment`
    /// (when comments are enabled).
    async fn update_linear(&mut self, task: &Task, target: LinearTarget, comment: Option<String>) {
        let Some(issue_id) = task.linear_issue_id().map(str::to_string) else { return };
        let Some(client) = self.linear_client() else { return };
        let state = match target {
            LinearTarget::InProgress => self.cfg.linear.in_progress_state.clone(),
            LinearTarget::Done => self.cfg.linear.done_state.clone(),
            LinearTarget::Blocked => self.cfg.linear.blocked_state.clone(),
        }
        .filter(|s| !s.trim().is_empty());
        if !self.cfg.linear.manage_states {
            if let Some(name) = &state {
                tracing::debug!(task = %task.key, state = %name, "linear.manage_states is off; leaving the issue state alone");
            }
        } else if let Some(name) = state {
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
        if let Some(body) = comment {
            self.comment_issue(task, body, CommentKind::Progress).await;
        }
    }

    /// Route comments to the task's tracker without changing its lifecycle state.
    async fn comment_issue(
        &mut self,
        task: &Task,
        body: String,
        kind: CommentKind,
    ) -> Option<Option<crate::linear::IssueComment>> {
        if matches!(&task.source, TaskSource::GitHub { .. }) {
            if !self.cfg.github.post_comments {
                return None;
            }
            self.update_github(task, None, Some(body)).await.then_some(None)
        } else {
            self.comment_linear(task, body, kind).await
        }
    }

    /// Best-effort comment on the task's Linear issue when
    /// `linear.post_comments` allows `kind`; never moves the issue. The body
    /// is tagged as powerqueue's own ([`relay::tag_own`]) and its id is
    /// remembered so the relay never hands it back to the session. Returns
    /// `None` when nothing was posted (not allowed, no client, failure) and
    /// `Some(comment)` once Linear confirmed it (`comment` when it returned one).
    async fn comment_linear(
        &mut self,
        task: &Task,
        body: String,
        kind: CommentKind,
    ) -> Option<Option<crate::linear::IssueComment>> {
        let issue_id = task.linear_issue_id().map(str::to_string)?;
        let allowed = match kind {
            CommentKind::Progress => self.cfg.linear.post_comments.progress(),
            CommentKind::Notice => self.cfg.linear.post_comments.questions(),
        };
        if !allowed {
            return None;
        }
        let client = self.linear_client()?;
        let body = relay::tag_own(&body);
        match client.post_comment(&issue_id, &body).await {
            Ok(posted) => {
                if let Some(c) = &posted {
                    self.update_relay(task, |state| state.record_posted(&c.id));
                }
                self.log(
                    Some(task.id),
                    None,
                    EventLevel::Debug,
                    "linear.comment",
                    "posted comment",
                    serde_json::json!({ "body": body, "id": posted.as_ref().map(|c| c.id.clone()) }),
                );
                Some(posted)
            }
            Err(e) => {
                self.log(
                    Some(task.id),
                    None,
                    EventLevel::Warn,
                    "linear.error",
                    &format!("could not post comment: {e:#}"),
                    serde_json::json!({}),
                );
                None
            }
        }
    }

    /// Read-modify-write the task's [`RelayState`] in kv (best effort).
    fn update_relay(&self, task: &Task, change: impl FnOnce(&mut RelayState)) {
        let key = relay_key(task.id);
        let mut state = match self.store.kv_get::<RelayState>(&key) {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(task = %task.key, error = %format!("{e:#}"), "cannot read relay state");
                RelayState::default()
            }
        };
        change(&mut state);
        if let Err(e) = self.store.kv_set(&key, &state) {
            tracing::warn!(task = %task.key, error = %format!("{e:#}"), "cannot persist relay state");
        }
    }

    /// Post the agent's question on the task's Linear issue (when
    /// `linear.post_comments` is not `false`), once per session and text,
    /// and start waiting for a reply ([`Daemon::relay_comments`]). An
    /// unconfirmed question (a turn that ended in `needs_attention`) is
    /// posted only when the session's agent ran `task block` itself.
    async fn post_question(&mut self, task: &Task, session_id: uuid::Uuid, text: &str, reason: Option<&str>, confirmed: bool) {
        let allowed = match &task.source {
            TaskSource::Linear { .. } => self.cfg.linear.post_comments.questions(),
            TaskSource::GitHub { .. } => self.cfg.github.post_comments,
            TaskSource::Manual => false,
        };
        if !allowed {
            return;
        }
        let state = self.store.kv_get::<RelayState>(&relay_key(task.id)).ok().flatten().unwrap_or_default();
        if !confirmed && state.agent_blocked != Some(session_id) {
            tracing::debug!(task = %task.key, session = %session_id, "needs_attention not raised by the agent; no question");
            return;
        }
        if state.already_asked(session_id, text) {
            tracing::debug!(task = %task.key, session = %session_id, "question already relayed");
            return;
        }
        let now = Utc::now();
        // `comment_linear` logs failures; the question is then not recorded.
        let Some(posted) = self.comment_issue(task, relay::question_body(text, reason), CommentKind::Notice).await else {
            return;
        };
        let posted_at = posted.as_ref().map_or(now, |c| c.created_at);
        let question =
            PostedQuestion { session_id, text: text.to_string(), comment_id: posted.map(|c| c.id), posted_at, answered: false };
        self.update_relay(task, |state| {
            state.seen_until = Some(state.seen_until.map_or(posted_at, |t| t.max(posted_at)));
            state.question = Some(question);
            state.pending_answer = None;
            state.pending_session = None;
            state.agent_blocked = None;
        });
        self.log(
            Some(task.id),
            Some(session_id),
            EventLevel::Info,
            "relay.question_posted",
            if matches!(&task.source, TaskSource::GitHub { .. }) {
                "asked the agent's question on the GitHub issue; respond with task send or attach"
            } else {
                "asked the agent's question on the Linear issue; a reply there goes back to the session"
            },
            serde_json::json!({ "question": text, "reason": reason }),
        );
    }

    /// Relay human comments on Linear issues to sessions, on the Linear
    /// poll cadence (when `linear.post_comments` is not `false`):
    ///
    /// * `needs_attention` / `in_review` with an unanswered question: the
    ///   replies newer than it are typed into the live session (the task
    ///   goes back to `running`), or kept as the pending answer and the task
    ///   re-queued, so the next launch resumes the session with them;
    /// * `running` / `idle`: new comments are typed in as a hint.
    ///
    /// Comments powerqueue posted are skipped. A failed Linear request backs
    /// off like the sync; a failed `tmux send-keys` is logged as
    /// `relay.error` (counted by `doctor`).
    async fn relay_comments(&mut self, now: DateTime<Utc>) -> Result<()> {
        if self.offline || !self.cfg.linear.enabled || !self.cfg.linear.post_comments.questions() {
            return Ok(());
        }
        let interval = Duration::seconds(self.cfg.linear.poll_interval_secs.max(5) as i64);
        if self.rt.last_relay_poll.is_some_and(|t| now - t < interval) || self.rt.linear_next_allowed.is_some_and(|t| now < t) {
            return Ok(());
        }
        self.rt.last_relay_poll = Some(now);
        let mut watched = Vec::new();
        for task in self.store.list_open_tasks()? {
            if task.linear_issue_id().is_none() {
                continue;
            }
            let state = self.store.kv_get::<RelayState>(&relay_key(task.id))?.unwrap_or_default();
            let live = self.store.latest_session(task.id)?.filter(|s| s.state.is_live());
            // Typing into a permission menu would pick an option.
            let at_permission_prompt = task.last_error.as_deref().is_some_and(|r| r.starts_with(transitions::PERMISSION_PREFIX));
            let target = match task.state {
                TaskState::NeedsAttention if at_permission_prompt => None,
                TaskState::NeedsAttention | TaskState::InReview => match state.open_question() {
                    Some(q) => {
                        let since = state.seen_until.map_or(q.posted_at, |t| t.max(q.posted_at));
                        Some((RelayMode::Answer, since))
                    }
                    None => None,
                },
                TaskState::Running | TaskState::Idle => {
                    live.as_ref().map(|s| (RelayMode::Hint, state.seen_until.unwrap_or(s.started_at)))
                }
                _ => None,
            };
            if let Some((mode, since)) = target {
                watched.push((task, mode, since));
            }
        }
        if watched.is_empty() {
            return Ok(());
        }
        let Some(client) = self.linear_client() else { return Ok(()) };
        for (task, mode, since) in watched {
            let Some(issue_id) = task.linear_issue_id() else { continue };
            let comments = match client.comments_since(issue_id, since).await {
                Ok(c) => c,
                Err(e) => {
                    self.linear_failure(now, &format!("reading comments of {} failed: {e:#}", task.key));
                    return Ok(());
                }
            };
            // The hook process and the CLI write tasks and the relay state
            // too: decide on fresh copies, and leave a task that changed
            // during the request to the next poll.
            let Some(task) = self.store.get_task(task.id)?.filter(|t| t.state == task.state) else { continue };
            let live = self.store.latest_session(task.id)?.filter(|s| s.state.is_live());
            let mut state = self.store.kv_get::<RelayState>(&relay_key(task.id))?.unwrap_or_default();
            let previous_seen = state.seen_until;
            let (human, newest) = state.human_comments(&comments, since);
            if let Some(newest) = newest {
                state.seen_until = Some(state.seen_until.map_or(newest, |t| t.max(newest)));
            }
            let ids: Vec<String> = human.iter().map(|c| c.id.clone()).collect();
            let mut requeue = None;
            if mode == RelayMode::Hint
                && let Some(q) = state.question.as_mut()
            {
                // The agent is working again (answered through tmux or
                // `task send`): its question is settled.
                q.answered = true;
            }
            if !human.is_empty() {
                match mode {
                    RelayMode::Hint => self.relay_hint(&task, live.as_ref(), &relay::hint_text(&human), &ids),
                    RelayMode::Answer => {
                        let text = relay::answer_text(&human);
                        match self.send_answer(&task, live.as_ref(), &text, &ids, now).await? {
                            Delivery::Sent => {
                                if let Some(q) = state.question.as_mut() {
                                    q.answered = true;
                                }
                            }
                            // The question stays open and the comments
                            // unseen: the next poll tries again.
                            Delivery::Failed => state.seen_until = previous_seen,
                            Delivery::NoSession => {
                                let asked = state.question.as_mut().map(|q| {
                                    q.answered = true;
                                    q.session_id
                                });
                                state.pending_answer = Some(text);
                                state.pending_session = asked;
                                requeue = Some(ids);
                            }
                        }
                    }
                }
            }
            // Saved before re-queuing, so the launch finds the pending answer.
            self.store.kv_set(&relay_key(task.id), &state).with_context(|| format!("save relay state of {}", task.key))?;
            if let Some(ids) = requeue {
                self.requeue_with_answer(task, &ids, now)?;
            }
        }
        Ok(())
    }

    /// Type a hint into a running session (best effort).
    fn relay_hint(&mut self, task: &Task, session: Option<&Session>, text: &str, ids: &[String]) {
        let Some((session, pane)) = session.and_then(|s| s.pane_id.as_deref().map(|p| (s, p))) else { return };
        match self.rt.tmux.send_text(pane, text) {
            Ok(()) => self.log(
                Some(task.id),
                Some(session.id),
                EventLevel::Info,
                "relay.hint_sent",
                "relayed a new Linear comment to the running session",
                serde_json::json!({ "comments": ids, "text": text }),
            ),
            Err(e) => self.log(
                Some(task.id),
                Some(session.id),
                EventLevel::Warn,
                "relay.error",
                &format!("could not type a Linear comment into pane {pane}: {e:#}"),
                serde_json::json!({ "comments": ids }),
            ),
        }
    }

    /// Type a reply into the live session of a `needs_attention` task and
    /// set it running again. Without a live session (`in_review`, parked)
    /// or when typing fails, the caller keeps the answer for the next launch.
    async fn send_answer(
        &mut self,
        task: &Task,
        session: Option<&Session>,
        text: &str,
        ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<Delivery> {
        let Some(session) = session.filter(|_| task.state == TaskState::NeedsAttention) else { return Ok(Delivery::NoSession) };
        let Some(pane) = session.pane_id.clone() else { return Ok(Delivery::Failed) };
        if let Err(e) = self.rt.tmux.send_text(&pane, text) {
            self.log(
                Some(task.id),
                Some(session.id),
                EventLevel::Warn,
                "relay.error",
                &format!("could not type the Linear reply into pane {pane}: {e:#}; it is kept for the next launch"),
                serde_json::json!({ "comments": ids }),
            );
            return Ok(Delivery::Failed);
        }
        let mut task = task.clone();
        let mut session = session.clone();
        let effects = transitions::on_answer_sent(&mut task, &mut session, text, ids, &self.cfg, now);
        self.store.update_session(&session)?;
        self.store.update_task(&task)?;
        self.rt.nudged.remove(&session.id);
        self.apply_effects(&mut task, Some(&session), effects).await;
        Ok(Delivery::Sent)
    }

    /// Re-queue a task whose session is gone so the next launch resumes it
    /// with the pending answer ([`RelayState::pending_answer`]).
    fn requeue_with_answer(&mut self, mut task: Task, ids: &[String], now: DateTime<Utc>) -> Result<()> {
        let before = task.clone();
        let effects = transitions::on_answer_queued(&mut task, ids, now);
        if task != before {
            self.store.update_task(&task)?;
        }
        self.log_only(&task, None, effects);
        Ok(())
    }

    // -------------------------------------------------------------- github

    fn github_client(&mut self) -> Option<GitHubClient> {
        if self.offline || !self.cfg.github.enabled {
            return None;
        }
        if self.rt.github.client.is_none() {
            let result =
                self.secrets.require(SecretKind::GitHubToken).and_then(|key| GitHubClient::new(&self.cfg.github.endpoint, key));
            match result {
                Ok(client) => self.rt.github.client = Some(client),
                Err(e) if !self.rt.github.warned => {
                    self.rt.github.warned = true;
                    self.log(
                        None,
                        None,
                        EventLevel::Warn,
                        "github.unavailable",
                        &format!("{e:#}; run `powerqueue secrets set github` and check github.endpoint"),
                        serde_json::json!({}),
                    );
                }
                Err(_) => {}
            }
        }
        self.rt.github.client.clone()
    }

    async fn poll_github(&mut self, now: DateTime<Utc>) -> Result<()> {
        let interval = Duration::seconds(self.cfg.github.poll_interval_secs.clamp(5, 86400) as i64);
        if self.rt.github.next_allowed.is_some_and(|t| now < t)
            || (!self.rt.force_sync && self.rt.github.last_poll.is_some_and(|t| now - t < interval))
        {
            return Ok(());
        }
        let Some(client) = self.github_client() else { return Ok(()) };
        self.rt.github.last_poll = Some(now);
        let plan = match plan_sync(&self.store, &self.cfg.github, &client).await {
            Ok(plan) => plan,
            Err(error) => {
                self.github_failure(&error);
                return Ok(());
            }
        };
        apply_plan(&self.store, &plan)?;
        self.rt.github.backoff = Duration::zero();
        self.rt.github.next_allowed = None;
        if !plan.changes.is_empty() {
            self.log(
                None,
                None,
                EventLevel::Info,
                "github.sync",
                &format!("GitHub sync: {} queue changes", plan.changes.len()),
                serde_json::json!({"fetched": plan.fetched, "changes": plan.changes.len()}),
            );
        }
        Ok(())
    }

    // ---------------------------------------------------------------- inbox

    /// Drain the task inboxes written by the session shim
    /// ([`crate::session::inbox`]) when a provider has `shim = true`: hook
    /// messages become `hook_events` rows (handled by [`Self::process_hooks`]
    /// right after), `task complete|block` messages are applied exactly as
    /// the CLI would apply them. Unparsable files move to `inbox/rejected/`
    /// (logged as `inbox.rejected`, reported by `doctor`); a message that
    /// cannot be applied is logged as `inbox.error` and dropped.
    async fn process_inbox(&mut self, now: DateTime<Utc>) -> Result<()> {
        // Not gated on the current `shim` settings: a session launched with
        // a shim keeps using it after a config reload turned the setting
        // off, and an inbox directory only exists for such sessions.
        let tasks = self.store.list_tasks()?;
        for task in tasks.iter().filter(|t| !t.state.is_terminal()) {
            self.drain_task_inbox(task, now)?;
        }
        Ok(())
    }

    /// Drain and apply the inbox of one task (see [`Self::process_inbox`]).
    /// Also called when a pane is found dead, so a `task complete` the
    /// session queued right before exiting is applied before the exit is
    /// read as a crash.
    fn drain_task_inbox(&self, task: &Task, now: DateTime<Utc>) -> Result<()> {
        use crate::session::inbox;
        let inbox_dir = inbox::inbox_dir(&self.paths.task_dir(&task.id.to_string()));
        if !inbox_dir.is_dir() {
            return Ok(());
        }
        for (path, parsed) in inbox::drain(&inbox_dir)? {
            let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            match parsed {
                Ok(msg) => {
                    let kind = msg.header.kind();
                    tracing::debug!(task = %task.key, kind, file = %file, "inbox message");
                    if let Err(e) = self.apply_inbox_message(task, &msg, now) {
                        tracing::warn!(task = %task.key, kind, error = %format!("{e:#}"), "inbox message could not be applied");
                        self.log(
                            Some(task.id),
                            msg.header.session(),
                            EventLevel::Error,
                            "inbox.error",
                            &format!("{kind} message from the session shim: {e:#}"),
                            serde_json::json!({ "kind": kind, "file": file }),
                        );
                    }
                    if let Err(e) = std::fs::remove_file(&path) {
                        tracing::warn!(file = %path.display(), error = %e, "cannot remove a drained inbox message");
                    }
                }
                Err(e) => {
                    let moved = inbox::reject(&path);
                    tracing::warn!(task = %task.key, file = %file, error = %format!("{e:#}"), "rejected an inbox message");
                    self.log(
                        Some(task.id),
                        None,
                        EventLevel::Warn,
                        "inbox.rejected",
                        &format!("unreadable message from the session shim ({file}): {e:#}"),
                        serde_json::json!({ "file": file, "moved_to": moved.ok().map(|p| p.display().to_string()) }),
                    );
                }
            }
        }
        Ok(())
    }

    fn github_failure(&mut self, error: &anyhow::Error) {
        let base = Duration::seconds(self.cfg.github.poll_interval_secs.clamp(5, 3600) as i64);
        self.rt.github.backoff =
            if self.rt.github.backoff.is_zero() { base } else { (self.rt.github.backoff * 2).min(Duration::hours(1)) };
        let mut next = Utc::now() + self.rt.github.backoff;
        if let Some(limit) = error.downcast_ref::<crate::github::client::RateLimited>() {
            next = next.max(limit.retry_at);
        }
        self.rt.github.next_allowed = Some(self.rt.github.next_allowed.map_or(next, |old| old.max(next)));
        self.log(
            None,
            None,
            EventLevel::Warn,
            "github.error",
            &format!("{error:#}; GitHub calls paused until {next}"),
            serde_json::json!({"retry_at": next}),
        );
    }

    async fn update_issue(&mut self, task: &Task, target: LinearTarget, comment: Option<String>) {
        if matches!(&task.source, TaskSource::GitHub { .. }) {
            self.update_github(task, Some(target), comment).await;
        } else {
            self.update_linear(task, target, comment).await;
        }
    }

    async fn update_github(&mut self, task: &Task, target: Option<LinearTarget>, comment: Option<String>) -> bool {
        let TaskSource::GitHub { repository, number, .. } = &task.source else { return false };
        // Configuration changes must not mutate issues in a previously configured repository.
        if !repository.eq_ignore_ascii_case(&self.cfg.github.repository) {
            return false;
        }
        if self.rt.github.next_allowed.is_some_and(|t| Utc::now() < t) {
            self.log(
                Some(task.id),
                None,
                EventLevel::Warn,
                "github.update_skipped",
                "GitHub lifecycle update skipped during API backoff; update the issue manually",
                serde_json::json!({}),
            );
            return false;
        }
        let Some(client) = self.github_client() else { return false };
        let cfg = self.cfg.github.clone();
        let label = match target {
            Some(LinearTarget::InProgress) => cfg.in_progress_label.as_deref(),
            Some(LinearTarget::Done) => cfg.done_label.as_deref(),
            Some(LinearTarget::Blocked) => cfg.blocked_label.as_deref(),
            None => None,
        };
        let result: Result<()> = async {
            if let Some(label) = label {
                let managed: Vec<_> = [cfg.in_progress_label.as_deref(), cfg.done_label.as_deref(), cfg.blocked_label.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect();
                client.set_label(repository, *number, label, &managed).await?;
                self.log(
                    Some(task.id),
                    None,
                    EventLevel::Info,
                    "github.label",
                    &format!("set lifecycle label {label}"),
                    serde_json::json!({"label": label}),
                );
            }
            if target == Some(LinearTarget::Done) && cfg.close_on_complete {
                client.close_issue(repository, *number).await?;
                self.log(Some(task.id), None, EventLevel::Info, "github.closed", "closed completed issue", serde_json::json!({}));
            }
            if cfg.post_comments
                && let Some(body) = comment
            {
                client.comment(repository, *number, &body).await?;
                self.log(Some(task.id), None, EventLevel::Info, "github.comment", "posted comment", serde_json::json!({}));
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            self.log(
                Some(task.id),
                None,
                EventLevel::Warn,
                "github.update_failed",
                &format!("GitHub lifecycle update failed: {error:#}; update the issue manually"),
                serde_json::json!({}),
            );
            self.github_failure(&error);
            return false;
        }
        true
    }

    /// Apply one shim message. `task` owns the inbox the message sat in;
    /// the message itself names the task the way `powerqueue task` accepts
    /// it (id, key or prefix), which must resolve to that same task.
    fn apply_inbox_message(&self, task: &Task, msg: &crate::session::inbox::InboxMessage, now: DateTime<Utc>) -> Result<()> {
        use crate::cli::commands::task::{block_task, complete_task, hand_off_for_review, record_agent_block};
        use crate::session::inbox::InboxHeader;
        let mut target =
            self.store.find_task(msg.header.task())?.ok_or_else(|| anyhow::anyhow!("no task matches `{}`", msg.header.task()))?;
        if target.id != task.id {
            bail!("the message names {} but sat in the inbox of {}", target.key, task.key);
        }
        let session = msg.header.session();
        // A message from a session that already ended (a Stop that arrived
        // after `task complete`, drained only once the task was re-queued)
        // must not act on the next attempt.
        let live = match session {
            Some(sid) => self.store.get_session(sid)?.map(|s| s.state.is_live()),
            None => self.store.latest_session(task.id)?.map(|s| s.state.is_live()),
        };
        if live == Some(false) {
            tracing::debug!(task = %task.key, kind = msg.header.kind(), ?session, "ignoring an inbox message from an ended session");
            self.log(
                Some(task.id),
                session,
                EventLevel::Debug,
                "inbox.stale",
                &format!("{} message from a session that already ended; ignored", msg.header.kind()),
                serde_json::json!({ "kind": msg.header.kind() }),
            );
            return Ok(());
        }
        let body = msg.body.trim();
        match &msg.header {
            InboxHeader::Hook { provider, event, .. } => {
                if *provider == Provider::Claude
                    && event.trim().eq_ignore_ascii_case(crate::budget::probes::claude::STATUS_LINE_EVENT)
                {
                    crate::hook::handle_status_line(Some(&self.store), &mut msg.body.as_bytes(), now);
                    return Ok(());
                }
                crate::hook::handle_provider(&self.store, *provider, target.id, session, event, &msg.body)?;
            }
            InboxHeader::Complete { pr, .. } => {
                let summary = (!body.is_empty()).then_some(body);
                match pr.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
                    Some(url) => hand_off_for_review(&self.store, &mut target, summary, url)?,
                    None => complete_task(&self.store, &mut target, summary)?,
                }
                self.log(
                    Some(target.id),
                    session,
                    EventLevel::Debug,
                    "inbox.complete",
                    "task complete received from the session shim",
                    serde_json::json!({ "pr": pr, "summary": summary }),
                );
            }
            InboxHeader::Block { .. } => {
                let reason = (!body.is_empty()).then_some(body);
                block_task(&self.store, &mut target, reason)?;
                let session_s = session.map(|s| s.to_string());
                record_agent_block(&self.store, &target, Some(&target.id.to_string()), session_s.as_deref())?;
                self.log(
                    Some(target.id),
                    session,
                    EventLevel::Debug,
                    "inbox.block",
                    "task block received from the session shim",
                    serde_json::json!({ "reason": reason }),
                );
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- hooks

    async fn process_hooks(&mut self, now: DateTime<Utc>) -> Result<()> {
        let events = self.store.drain_hook_events()?;
        if events.is_empty() {
            return Ok(());
        }
        for ev in events {
            if let Err(e) = self.process_hook(&ev, now).await {
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

    async fn process_hook(&mut self, ev: &PendingHookEvent, now: DateTime<Utc>) -> Result<()> {
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
        let period_end = self.provider_period_end(session.model.provider(), now);
        let outcome = interpret_hook(ev.event, &ev.payload);
        tracing::debug!(task = %task.key, session = %session.id, event = ev.event.as_str(), ?outcome, "hook outcome");
        let session_was_live = session.state.is_live();
        let mut effects = transitions::on_hook_outcome(&mut task, &mut session, &outcome, &self.cfg, now, period_end);
        // Only the hook that ends the session of a completed task: later
        // ones (duplicate Stop, SessionEnd) come after its cleanup ran.
        if session_was_live
            && !session.state.is_live()
            && task.state == TaskState::Completed
            && self.adopt_open_pr(&mut task, now)
        {
            // The agent finished without `--pr` but its branch has an open
            // PR: release the session for review instead of completing.
            effects = transitions::release_for_review(&task);
        }
        if task.state == TaskState::Running && session.state == SessionState::Running {
            self.rt.nudged.remove(&session.id);
        }
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
                    if let Err(e) = cleanup_task(&self.cfg, &self.store, &self.rt.repo, &self.rt.tmux, task, succeeded, false) {
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
                Effect::Linear { target, comment } => self.update_issue(task, target, comment).await,
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
                    if let Some(s) = session {
                        if let Err(e) = self.rt.tmux.kill_window(&s.tmux_window) {
                            tracing::debug!(task = %task.key, window = %s.tmux_window, error = %format!("{e:#}"), "kill window");
                        }
                        self.forget_session(s.id);
                    }
                }
                Effect::RateLimit { tier, until } => {
                    self.rt.rate_limits.mark(tier, until);
                    if let Err(e) = self.store.kv_set(RATE_LIMITS_KEY, &self.rt.rate_limits) {
                        tracing::warn!(error = %e, "cannot persist rate limits");
                    }
                }
                Effect::RateLimitProvider { provider, until } => {
                    self.rt.rate_limits.mark_provider(&self.cfg.budget, provider, until);
                    if let Err(e) = self.store.kv_set(RATE_LIMITS_KEY, &self.rt.rate_limits) {
                        tracing::warn!(error = %e, "cannot persist rate limits");
                    }
                }
                Effect::ReleaseForReview => {
                    if let Some(s) = session
                        && let Err(e) = self.rt.tmux.kill_window(&s.tmux_window)
                    {
                        tracing::debug!(task = %task.key, window = %s.tmux_window, error = %format!("{e:#}"), "kill window for review");
                    }
                    if let Some(watch) = task.review.as_mut()
                        && watch.worktree_path.is_none()
                    {
                        watch.worktree_path = task.worktree_path.clone();
                    }
                    if let Err(e) = cleanup_task(&self.cfg, &self.store, &self.rt.repo, &self.rt.tmux, task, true, true) {
                        self.log(
                            Some(task.id),
                            session_id,
                            EventLevel::Warn,
                            "cleanup.error",
                            &format!("cleanup for review failed: {e:#}"),
                            serde_json::json!({}),
                        );
                    }
                    if let Some(id) = session_id {
                        self.forget_session(id);
                    }
                }
                Effect::LinearComment { body } => {
                    self.comment_issue(task, body, CommentKind::Notice).await;
                }
                Effect::Question { text, reason, confirmed } => {
                    if let Some(s) = session {
                        self.post_question(task, s.id, &text, reason.as_deref(), confirmed).await;
                    }
                }
                Effect::ProgressComment { body } => {
                    self.comment_issue(task, body, CommentKind::Progress).await;
                }
                Effect::ForgetAnswer => self.update_relay(task, |state| {
                    state.pending_answer = None;
                    state.pending_session = None;
                }),
                Effect::DeleteBranch => {
                    let Some(branch) = task.branch.clone() else { continue };
                    if let Some(wt) = task.worktree_path.clone() {
                        // Kept earlier (e.g. nothing was pushed); the PR is merged now.
                        match self.rt.repo.remove_worktree(Path::new(&wt), false) {
                            Ok(()) => task.worktree_path = None,
                            Err(e) => {
                                self.log(
                                    Some(task.id),
                                    session_id,
                                    EventLevel::Warn,
                                    "cleanup.remove_failed",
                                    &format!("PR merged but the worktree {wt} could not be removed; keeping {branch}: {e:#}"),
                                    serde_json::json!({ "path": wt }),
                                );
                                continue;
                            }
                        }
                    }
                    match self.rt.repo.delete_branch(&branch, true) {
                        Ok(()) => self.log(
                            Some(task.id),
                            session_id,
                            EventLevel::Info,
                            "cleanup.branch_deleted",
                            &format!("deleted local branch {branch} (its PR was merged)"),
                            serde_json::json!({ "branch": branch }),
                        ),
                        Err(e) => self.log(
                            Some(task.id),
                            session_id,
                            EventLevel::Warn,
                            "cleanup.branch_delete_failed",
                            &format!("could not delete {branch}: {e:#}"),
                            serde_json::json!({ "branch": branch }),
                        ),
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

    async fn tail_transcripts(&mut self, now: DateTime<Utc>) -> Result<()> {
        if let Err(e) = self.discover_agent_sessions(now) {
            tracing::warn!(error = %format!("{e:#}"), "agent session discovery failed");
        }
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
                    let guess = transcript_path_for(&claude_home(&self.cfg), Path::new(wt), session.id);
                    if !guess.exists() {
                        continue;
                    }
                    session.transcript_path = Some(guess.display().to_string());
                    self.store.update_session(&session)?;
                    guess
                }
            };
            let reader = self.rt.readers.entry(session.id).or_insert_with(|| {
                TranscriptReader::for_provider(path, session.id, session.task_id, session.model.provider(), session.model.clone())
            });
            let offset_before = reader.offset;
            let records = match reader.read_new() {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(session = %session.id, error = %format!("{e:#}"), "transcript read failed");
                    continue;
                }
            };
            // New lines without usage (e.g. Antigravity transcripts) still count as activity.
            let advanced = reader.offset != offset_before;
            let newest_line = reader.last_line_at;
            // A restart replays the whole transcript. Only responses newer
            // than the last hook/activity may clear an outstanding blocker.
            let progressed = advanced && reader.last_progress_at.is_some_and(|at| at > session.last_activity_at && at <= now);
            // A Codex rollout carries a rate-limit snapshot in every
            // `token_count`; treat it like a probe result so live sessions
            // keep the observed usage fresh between probe runs.
            if let Some(snapshot) = reader.state.rate_limits.take()
                && let Some(observed) = crate::budget::probes::codex::parse_rate_limits(&snapshot)
            {
                let provider = reader.provider;
                if let Err(e) = crate::budget::save_observed(&self.store, provider, &observed) {
                    tracing::warn!(session = %session.id, error = %format!("{e:#}"), "cannot store observed usage from the transcript");
                } else {
                    tracing::debug!(session = %session.id, %provider, "observed usage refreshed from the transcript");
                }
            }
            let rate_limit_errors = reader.state.take_rate_limit_errors();
            let polled = if agent_for(reader.provider).polls_transcript_for_completion() {
                reader.state.take_pending_message()
            } else {
                None
            };
            self.forward_transcript_signals(&session, rate_limit_errors, polled)?;
            if records.is_empty() && !advanced {
                continue;
            }
            let mut inserted = 0usize;
            let mut weighted = 0.0;
            for rec in &records {
                if self.store.record_usage(rec)? {
                    inserted += 1;
                    weighted += rec.usage.weighted() * tier_weight(&self.cfg.budget, &rec.tier);
                }
            }
            let newest = newest_line.unwrap_or(now).min(now);
            if session.state.is_live() {
                if progressed && let Some(mut task) = self.store.get_task(session.task_id)? {
                    let effects = transitions::on_progress(&mut task, &mut session);
                    self.store.update_task(&task)?;
                    self.apply_effects(&mut task, Some(&session), effects).await;
                    if task.state == TaskState::Running {
                        self.rt.nudged.remove(&session.id);
                    }
                }
                transitions::on_transcript_activity(&mut session, newest);
                self.store.update_session(&session)?;
            }
            tracing::debug!(session = %session.id, inserted, weighted, "recorded transcript usage");
        }
        Ok(())
    }

    /// For sessions of CLIs that generate their own session id (Codex
    /// thread, Antigravity conversation) that are live or ended within the
    /// transcript grace period, ask the provider to find the id and record
    /// it with its transcript path (`session.discovered`). Usage of a session
    /// that finished between two ticks is then still read, and crash
    /// restarts resume that id.
    fn discover_agent_sessions(&mut self, now: DateTime<Utc>) -> Result<()> {
        for session in self.store.list_sessions_active_since(now - TRANSCRIPT_GRACE)? {
            self.discover_agent_session(session)?;
        }
        Ok(())
    }

    /// Discovery for one session; returns the session as stored afterwards.
    fn discover_agent_session(&mut self, mut session: Session) -> Result<Session> {
        let agent = agent_for(session.model.provider());
        if session.agent_session_id.is_some() || agent.accepts_session_id() {
            return Ok(session);
        }
        let Some(task) = self.store.get_task(session.task_id)? else { return Ok(session) };
        let Some(wt) = task.worktree_path.as_deref() else { return Ok(session) };
        match agent.discover_session(&self.cfg, Path::new(wt), session.started_at) {
            Ok(Some((id, path))) => {
                session.agent_session_id = Some(id.clone());
                session.transcript_path = Some(path.display().to_string());
                self.store.update_session(&session)?;
                self.log(
                    Some(task.id),
                    Some(session.id),
                    EventLevel::Info,
                    "session.discovered",
                    &format!("{} session {id} found", session.model.provider()),
                    serde_json::json!({ "agent_session_id": id, "transcript_path": path, "provider": session.model.provider() }),
                );
            }
            Ok(None) => {}
            Err(e) => tracing::debug!(task = %task.key, error = %format!("{e:#}"), "session discovery failed"),
        }
        Ok(session)
    }

    /// Turn what a transcript says into hook events the hook phase handles
    /// on the next tick: throttling errors (Codex `event_msg/error`) become a
    /// `StopFailure{rate_limit}`, and for CLIs without a reliable `Stop` hook
    /// a final message carrying the DONE / BLOCKED marker becomes a `Stop`.
    fn forward_transcript_signals(
        &self,
        session: &Session,
        rate_limit_errors: Vec<String>,
        polled_message: Option<String>,
    ) -> Result<()> {
        if !session.state.is_live() {
            return Ok(());
        }
        for message in rate_limit_errors {
            let payload = serde_json::json!({
                "error_type": "rate_limit",
                "error": "rate_limit",
                "error_message": message,
                "source": "transcript",
            });
            self.store.insert_hook_event(session.task_id, Some(session.id), crate::domain::HookEvent::StopFailure, &payload)?;
            self.log(
                Some(session.task_id),
                Some(session.id),
                EventLevel::Warn,
                "session.rate_limit_detected",
                &format!("transcript reports a rate limit: {message}"),
                serde_json::json!({ "provider": session.model.provider(), "message": message }),
            );
        }
        if let Some(message) =
            polled_message.filter(|m| m.contains(crate::domain::DONE_MARKER) || m.contains(crate::domain::BLOCKED_MARKER))
        {
            let payload = serde_json::json!({ "last_assistant_message": message, "source": "transcript" });
            self.store.insert_hook_event(session.task_id, Some(session.id), crate::domain::HookEvent::Stop, &payload)?;
            self.log(
                Some(session.task_id),
                Some(session.id),
                EventLevel::Info,
                "session.marker_polled",
                "completion marker found in the transcript",
                serde_json::json!({ "provider": session.model.provider() }),
            );
        }
        Ok(())
    }

    // --------------------------------------------------------------- probes

    async fn probe_sessions(&mut self, now: DateTime<Utc>) -> Result<()> {
        for mut session in self.store.list_live_sessions()? {
            let Some(mut task) = self.store.get_task(session.task_id)? else {
                transitions::orphan_session(&mut session, now);
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
            // A shim session may have queued `task complete` and exited
            // between the inbox phase and this probe: apply what it left
            // before reading the dead pane as a crash.
            if !probe.is_alive() {
                self.drain_task_inbox(&task, now)?;
                if let Some(fresh) = self.store.get_task(task.id)? {
                    task = fresh;
                }
            }
            let sc = &self.cfg.scheduler;
            let silent = now - session.last_activity_at;
            let stale = matches!(task.state, TaskState::Running | TaskState::Starting)
                && sc.stale_session_secs > 0
                && silent > Duration::seconds(sc.stale_session_secs as i64);
            let timeout = transitions::session_timed_out(&task, &session, sc, now);
            let dying = !(probe.is_alive() || task.state.is_handed_off() || task.state == TaskState::Paused);
            let pane_tail = if dying || stale || timeout {
                session.pane_id.as_deref().and_then(|p| self.rt.tmux.capture_pane(p, CRASH_TAIL_LINES).ok())
            } else {
                None
            };
            let provider = session.model.provider();
            let mut provider_cooldown_until = None;
            if (stale || timeout) && pane_tail.as_deref().is_some_and(transitions::pane_waits_for_usage_reset) {
                self.rt.rate_limits.clear_expired(now);
                provider_cooldown_until = match self.provider_cooldown(provider, now) {
                    Some(until) => Some(until),
                    // The agent hit its limit without a StopFailure hook: the
                    // account is throttled, so cool the provider down.
                    None => Some(self.start_provider_cooldown(provider, &task, "pane shows the usage limit was reached", now)),
                };
            }
            let ctx = ProbeContext { now, nudged: self.rt.nudged.contains(&session.id), pane_tail, provider_cooldown_until };
            let wait_before = (session.waiting_since, session.waited_secs);
            let effects = transitions::on_probe(&mut task, &mut session, &probe, &self.cfg.scheduler, &ctx);
            if effects.is_empty() {
                // A wait on a human started or ended: keep the bookkeeping.
                if (session.waiting_since, session.waited_secs) != wait_before {
                    self.store.update_session(&session)?;
                }
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

    /// A task the agent completed without `task complete --pr` whose branch
    /// has an open PR goes `in_review` with that PR instead, so the watcher
    /// follows it to the merge (AVS-1652; also a review round that ends with
    /// only the done marker). Returns whether it did; a failed `gh` call
    /// (not installed, not a GitHub repo) is logged at debug level and
    /// leaves the task completed.
    fn adopt_open_pr(&mut self, task: &mut Task, now: DateTime<Utc>) -> bool {
        if task.state != TaskState::Completed || self.cfg.scheduler.pr_poll_secs == 0 {
            return false;
        }
        let Some(branch) = task.branch.clone() else { return false };
        let dir = task.worktree_path.clone().map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(|| self.cfg.repo_path());
        let gh = Gh::new(&self.cfg.scheduler.gh_binary);
        let url = match gh.open_pr_for_branch(&dir, &branch) {
            Ok(Some(url)) => url,
            Ok(None) => return false,
            Err(e) => {
                self.log(
                    Some(task.id),
                    None,
                    EventLevel::Debug,
                    "review.lookup_failed",
                    &format!("cannot tell whether {branch} has an open PR; completing the task: {e:#}"),
                    serde_json::json!({ "branch": branch }),
                );
                return false;
            }
        };
        task.hand_off_for_review(&url, now);
        self.log(
            Some(task.id),
            None,
            EventLevel::Info,
            "task.in_review",
            &format!("completed without a PR link, but {branch} has open PR {url}; handed off for review (merge armed)"),
            serde_json::json!({ "pr": url, "from": TaskState::Completed, "adopted": true }),
        );
        true
    }

    /// Tasks finished outside the hook path (`powerqueue task complete`,
    /// cancelled by Linear sync, ...) still have a live session: release it.
    /// A task handed off `in_review` gives up its window, slot and worktree
    /// but keeps its branch.
    async fn finalize_terminal(&mut self, now: DateTime<Utc>) -> Result<()> {
        for mut session in self.store.list_live_sessions()? {
            let Some(mut task) = self.store.get_task(session.task_id)? else { continue };
            if task.state == TaskState::Completed && self.adopt_open_pr(&mut task, now) {
                self.store.update_task(&task)?;
            }
            let effects = transitions::release_session(&task, &mut session, now);
            if effects.is_empty() {
                continue;
            }
            self.store.update_session(&session)?;
            self.apply_effects(&mut task, Some(&session), effects).await;
            self.store.update_task(&task)?;
            self.forget_session(session.id);
        }
        Ok(())
    }

    // -------------------------------------------------------------- reviews

    /// Poll the pull request of every `in_review` task that is due
    /// (`scheduler.pr_poll_secs`; 0 disables the watcher) and apply
    /// [`review::on_pr_status`]. A failed `gh` call never changes the task:
    /// it is noted in `last_error` (logged as `review.error` when the message
    /// changes) and retried at the next interval.
    async fn watch_reviews(&mut self, now: DateTime<Utc>) -> Result<()> {
        let interval = self.cfg.scheduler.pr_poll_secs;
        if interval == 0 {
            return Ok(());
        }
        let gh = Gh::new(&self.cfg.scheduler.gh_binary);
        for mut task in self.store.list_tasks_in_states(&[TaskState::InReview])? {
            let due =
                task.review.as_ref().and_then(|r| r.last_polled_at).is_none_or(|t| now - t >= Duration::seconds(interval as i64));
            if !due {
                continue;
            }
            // Handed off since this tick's finalize phase: release the
            // session first (next tick), so a merge seen now never races it.
            if self.store.latest_session(task.id)?.is_some_and(|s| s.state.is_live()) {
                continue;
            }
            let url = task.pr_url.clone().unwrap_or_default();
            let pr = match url.parse::<PrRef>() {
                Ok(pr) => pr,
                Err(e) => {
                    let effects = review::on_unusable_pr(&mut task, &e, now);
                    self.store.update_task(&task)?;
                    self.log_only(&task, None, effects);
                    continue;
                }
            };
            let status = match gh.pr_status(&pr) {
                Ok(status) => status,
                Err(e) => {
                    let effects = review::on_watch_error(&mut task, &pr.to_string(), &format!("{e:#}"), now);
                    self.store.update_task(&task)?;
                    self.log_only(&task, None, effects);
                    continue;
                }
            };
            review::on_watch_recovered(&mut task);
            let effects = review::on_pr_status(&mut task, &status, &self.cfg.scheduler, now);
            self.store.update_task(&task)?;
            self.apply_effects(&mut task, None, effects).await;
            if task.state == TaskState::Completed && matches!(&task.source, TaskSource::GitHub { .. }) {
                let comment = format!(
                    "powerqueue completed this task: PR {} merged.\n\n{}",
                    task.pr_url.as_deref().unwrap_or("?"),
                    task.summary.as_deref().unwrap_or("")
                );
                self.update_issue(&task, LinearTarget::Done, Some(comment)).await;
            }
            self.store.update_task(&task)?;
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

    // --------------------------------------------------------- usage probes

    /// Start the enabled providers' usage probes at the first tick and then
    /// every `budget.probe_interval_mins` (0 disables them). They run on a
    /// blocking thread and record their own results (kv + `budget.probe`
    /// events), so a slow CLI never delays the tick; a new round starts only
    /// after the previous one finished.
    fn run_usage_probes(&mut self, now: DateTime<Utc>) {
        if self.rt.usage_probe.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        self.rt.usage_probe = None;
        let interval = self.cfg.budget.probe_interval_mins;
        if interval == 0 {
            return;
        }
        let due = self.rt.last_usage_probe.is_none_or(|t| now - t >= Duration::minutes(interval as i64));
        if !due {
            return;
        }
        self.rt.last_usage_probe = Some(now);
        let cfg = self.cfg.clone();
        let store = self.store.clone();
        self.rt.usage_probe = Some(tokio::task::spawn_blocking(move || {
            for (provider, result) in probe_all(&cfg, &store, Utc::now()) {
                match result {
                    Ok(Some(_)) => tracing::debug!(provider = %provider, "usage probe learned something"),
                    Ok(None) => tracing::debug!(provider = %provider, "usage probe learned nothing"),
                    Err(e) => tracing::debug!(provider = %provider, error = %format!("{e:#}"), "usage probe failed"),
                }
            }
        }));
    }

    // --------------------------------------------------------------- launch

    /// What the planner decides with, borrowed per step rather than copied:
    /// a failed start may mark a rate limit the next candidate must see.
    fn launch_context(&self) -> LaunchContext<'_> {
        LaunchContext { budget: &self.cfg.budget, rules: &self.rt.rules, rate_limits: &self.rt.rate_limits }
    }

    async fn launch_tasks(&mut self, now: DateTime<Utc>) -> Result<()> {
        if let Some(pause) = self.store.scheduling_pause()? {
            tracing::debug!(since = %pause.since, "scheduling paused; launching nothing");
            return Ok(());
        }
        let live = self.store.list_live_sessions()?;
        let slots = self.cfg.scheduler.max_concurrent.saturating_sub(live.len() as u32);
        if slots == 0 {
            return Ok(());
        }
        let busy: HashSet<TaskId> = live.iter().map(|s| s.task_id).collect();
        let tasks: Vec<Task> = self.store.list_open_tasks()?.into_iter().filter(|t| !busy.contains(&t.id)).collect();
        if !tasks.iter().any(|t| t.state.is_schedulable()) {
            return Ok(());
        }
        let estimator = Estimator::from_summaries(&self.store.task_usage_summaries()?);
        let ledgers = Ledgers::load(&self.store, &self.cfg.budget, now)?;
        self.rt.rate_limits.clear_expired(now);
        let mut planner = LaunchPlanner::new(estimator, ledgers, tasks, slots);
        while let Some(candidate) = planner.next(self.launch_context(), now) {
            let Candidate { task, decision, .. } = candidate.clone();
            match decision.model.clone() {
                None => self.throttle(task, &decision, now)?,
                Some(model) => {
                    let (key, id) = (task.key.clone(), task.id);
                    match self.start_task(task, model, &decision, now).await {
                        Ok(true) => planner.started(&self.cfg.budget, &candidate),
                        // The start failed before the launch (worktree,
                        // setup, launcher) and the task is crashed with a
                        // backoff: the slot and the budget go to the next
                        // candidate of this pass.
                        Ok(false) => {}
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
        let before = task.clone();
        let effects = launch::on_throttled(&mut task, decision, now);
        if effects.is_empty() {
            tracing::debug!(task = %task.key, retry_at = ?task.not_before, "still throttled");
        }
        if task != before {
            self.store.update_task(&task)?;
        }
        self.log_only(&task, None, effects);
        Ok(())
    }

    /// Create the worktree, run setup, launch the agent and record the
    /// session. `Ok(true)` when the session launched; `Ok(false)` when the
    /// start failed before that and the task was crashed instead (its slot
    /// is free again). `Err` only for store failures.
    async fn start_task(&mut self, mut task: Task, model: ModelTier, decision: &Decision, now: DateTime<Utc>) -> Result<bool> {
        // A review round resumes the session that armed the merge, in the
        // same worktree path, told only what changed on the PR.
        let relaunch = task.review_relaunch().cloned();
        // A reply relayed from Linear while the session was gone: resume
        // the session with it as the prompt.
        let latest = self.store.latest_session(task.id)?.map(|s| s.id);
        let answer = self.store.kv_get::<RelayState>(&relay_key(task.id))?.and_then(|r| r.answer_for(latest).map(str::to_string));
        let root = self.cfg.worktree_root(&self.paths);
        let before = task.clone();
        let start = launch::on_starting(&mut task, &model, decision, &self.cfg, &root, relaunch.is_some() || answer.is_some());
        self.commit(&before, &mut task, None, start.effects.clone()).await?;
        // On the task from here on, persisted by the launch or by the crash
        // that prevents it (never by a `starting` row alone).
        launch::claim(&mut task, &start);
        let launch::Start { branch, worktree, attempt, .. } = start;

        let prepared = self.prepare_worktree(&task, &root, &worktree, &branch);
        if let Ok(true) = prepared {
            // The periodic fetch has nothing to add right after this one.
            self.rt.last_fetch = Some(now);
        }
        if let Err(e) = prepared {
            let before = task.clone();
            let effects = transitions::on_crash(
                &mut task,
                None,
                &format!("worktree setup failed: {e:#}"),
                None,
                &self.cfg.scheduler,
                now,
                None,
            );
            self.commit(&before, &mut task, None, effects).await?;
            return Ok(false);
        }

        let mut previous = self.store.latest_session(task.id)?;
        if let Some(p) = previous.take() {
            // Last chance to learn the id of a crashed session before resuming it.
            previous = Some(if p.state == SessionState::Crashed { self.discover_agent_session(p)? } else { p });
        }
        let transcript_present =
            previous.as_ref().and_then(|p| p.transcript_path.as_deref()).is_none_or(|p| Path::new(p).exists());
        if let Some(p) = previous.as_ref().filter(|p| p.state == SessionState::Crashed && !transcript_present) {
            self.log(
                Some(task.id),
                Some(p.id),
                EventLevel::Info,
                "session.fresh",
                "crashed session left no transcript to resume; starting a new one",
                serde_json::json!({ "previous": p.id, "transcript_path": p.transcript_path }),
            );
        }
        let (session_id, resume, resume_id) =
            launch::resume_plan(previous.as_ref(), &model, transcript_present, relaunch.is_some() || answer.is_some());
        let review_prompt = relaunch
            .as_ref()
            .map(|r| review::review_prompt(&self.cfg.scheduler.review_prompt, r, task.pr_url.as_deref().unwrap_or_default()));
        let prompts = launch::resume_prompt(
            review_prompt.as_deref(),
            answer.as_deref(),
            resume,
            task.pr_url.as_deref(),
            task.last_error.as_deref(),
        );
        let prepared = self.rt.launcher.prepare_with_prompt(
            &self.cfg,
            &task,
            session_id,
            &model,
            attempt,
            resume,
            resume_id.as_deref(),
            prompts.previous_error.as_deref(),
            prompts.prompt_override.as_deref(),
        );
        let launched = match prepared {
            Ok(plan) => {
                for warning in &plan.prompt_warnings {
                    self.log(
                        Some(task.id),
                        Some(session_id),
                        EventLevel::Warn,
                        "prompt.template_error",
                        warning,
                        serde_json::json!({ "template": plan.prompt_template, "configured": self.cfg.prompt.template_path() }),
                    );
                }
                self.rt.launcher.launch(&self.cfg, &task, &plan, session_id, &model, attempt)
            }
            Err(e) => Err(e),
        };
        let mut session = match launched {
            Ok(s) => s,
            Err(e) => {
                let before = task.clone();
                let effects = transitions::on_crash(
                    &mut task,
                    None,
                    &format!("launch failed: {e:#}"),
                    None,
                    &self.cfg.scheduler,
                    now,
                    None,
                );
                self.commit(&before, &mut task, None, effects).await?;
                return Ok(false);
            }
        };
        if resume && let Some(p) = previous.as_ref().filter(|p| p.id == session.id) {
            // The provider keeps writing the same transcript under the same id.
            session.agent_session_id = resume_id.clone();
            if session.transcript_path.is_none() {
                session.transcript_path = p.transcript_path.clone();
            }
            self.store.update_session(&session)?;
        } else {
            self.store.insert_session(&session)?;
        }
        self.forget_session(session.id);

        // The row may have been written while the launch ran (a command
        // applied offline during a long `repo.setup`, with the heartbeat
        // gone stale): never persist `running` over it. The launched
        // session is left to the finalize and probe phases like any session
        // of a task that moved on.
        if let Some(fresh) = self.store.get_task(task.id)?
            && fresh.state != TaskState::Starting
        {
            let before = fresh.clone();
            let mut fresh = fresh;
            let effects = launch::on_launch_superseded(&mut fresh, &task, &session);
            self.commit(&before, &mut fresh, Some(&session), effects).await?;
            return Ok(true);
        }

        let before = task.clone();
        let facts = launch::Launched {
            model: &model,
            attempt,
            resume,
            relaunch: relaunch.as_ref(),
            review_prompt: review_prompt.as_deref(),
            resume_prompt: prompts.resume_prompt.as_deref(),
            branch: &branch,
            worktree: &worktree,
            answered: answer.is_some(),
        };
        let effects = launch::on_launched(&mut task, &session, &facts, &self.cfg, now);
        self.commit(&before, &mut task, Some(&session), effects).await?;
        Ok(true)
    }

    /// Fast-forward the local branch `base` to `origin/<base>`: the default
    /// branch so people working in the main checkout see merged work too, or
    /// a reused task branch so a review round starts from commits pushed to
    /// the PR since the hand-off. Never fails the task; a skip is logged at
    /// debug level, a git failure as `repo.fast_forward_failed` (counted by
    /// `doctor`).
    fn fast_forward_base(&self, task: &Task, base: &str) {
        let (level, kind, message, data) = match self.rt.repo.fast_forward_branch(base) {
            Ok(FastForward::UpToDate) => return,
            Ok(FastForward::Updated { from, to }) => (
                EventLevel::Info,
                "repo.fast_forward",
                format!("fast-forwarded {base} from {} to {}", short_sha(&from), short_sha(&to)),
                serde_json::json!({ "branch": base, "from": from, "to": to }),
            ),
            Ok(FastForward::Skipped(why)) => (
                EventLevel::Debug,
                "repo.fast_forward_skipped",
                format!("left {base} where it is: {why}"),
                serde_json::json!({ "branch": base, "reason": why }),
            ),
            Err(e) => (
                EventLevel::Warn,
                "repo.fast_forward_failed",
                format!("fast-forward of {base} to origin/{base} failed: {e:#}"),
                serde_json::json!({ "branch": base, "error": format!("{e:#}") }),
            ),
        };
        self.log(Some(task.id), None, level, kind, &message, data);
    }

    /// Fetch, create (or reuse) the worktree and run `repo.setup`.
    ///
    /// A new branch starts from `origin/<base>` after a successful fetch, so
    /// work merged on the remote (a finished blocker's PR) is in it even when
    /// the local base branch lags; with fetching turned off, from the local
    /// base. When the fetch fails it starts from the newer of the last
    /// fetched `origin/<base>` and the local base, and `worktree.stale_base`
    /// is logged. Creating the branch logs [`BRANCH_CREATED_EVENT`] with the
    /// base right away, so it is known even if `repo.setup` fails. An
    /// existing branch (relaunch, review round) is reused, fast-forwarded
    /// to `origin/<branch>` after a successful fetch (commits pushed to the
    /// PR on GitHub since the hand-off); a branch with local commits the
    /// remote lacks is left as is.
    fn prepare_worktree(&self, task: &Task, root: &Path, worktree: &Path, branch: &str) -> Result<bool> {
        std::fs::create_dir_all(root).with_context(|| format!("create worktree root {}", root.display()))?;
        let new_branch = !self.rt.repo.branch_exists(branch).with_context(|| format!("look up branch {branch}"))?;
        let fetch_error =
            if self.cfg.repo.fetch_before_start { self.rt.repo.fetch().err().map(|e| format!("{e:#}")) } else { None };
        let fetched = self.cfg.repo.fetch_before_start && fetch_error.is_none();
        if let (Some(e), false) = (&fetch_error, new_branch) {
            self.log(
                Some(task.id),
                None,
                EventLevel::Warn,
                "repo.fetch_failed",
                &format!("git fetch failed; continuing: {e}"),
                serde_json::json!({}),
            );
        }
        let start = if new_branch {
            let base = match &self.cfg.repo.default_branch {
                Some(b) => b.clone(),
                None => self.rt.repo.default_branch().context("detect the default branch (set repo.default_branch)")?,
            };
            let prefer = match (self.cfg.repo.fetch_before_start, &fetch_error) {
                (false, _) => BasePreference::Local,
                (true, Some(_)) => BasePreference::Newest,
                (true, None) => BasePreference::Remote,
            };
            if prefer == BasePreference::Remote && self.cfg.repo.fast_forward_base {
                self.fast_forward_base(task, &base);
            }
            Some(self.rt.repo.start_point(&base, prefer)?)
        } else {
            if self.cfg.repo.fetch_before_start && fetch_error.is_none() {
                self.fast_forward_base(task, branch);
            }
            None
        };
        self.rt
            .repo
            .add_worktree_on(worktree, branch, start.as_ref().map(|s| s.sha.as_str()))
            .with_context(|| format!("create worktree {} on {branch}", worktree.display()))?;
        if let Some(s) = &start {
            self.log(
                Some(task.id),
                None,
                EventLevel::Info,
                BRANCH_CREATED_EVENT,
                &format!("created {branch} from {} at {}", s.reference, short_sha(&s.sha)),
                serde_json::json!({
                    "branch": branch,
                    "base": s.reference,
                    "base_sha": s.sha,
                    "stale_base": fetch_error.is_some(),
                }),
            );
            if let Some(e) = &fetch_error {
                self.log(
                    Some(task.id),
                    None,
                    EventLevel::Warn,
                    "worktree.stale_base",
                    &format!(
                        "git fetch failed; {branch} starts from {} at {}, which may lack recently merged work: {e}",
                        s.reference,
                        short_sha(&s.sha)
                    ),
                    serde_json::json!({ "branch": branch, "base": s.reference, "base_sha": s.sha, "error": e }),
                );
            }
        }
        if !self.cfg.repo.setup.is_empty() {
            let env = vec![
                ("POWERQUEUE_TASK_ID".to_string(), task.id.to_string()),
                ("POWERQUEUE_TASK_KEY".to_string(), task.key.clone()),
                ("POWERQUEUE_TASK_SLUG".to_string(), task.slug()),
                ("POWERQUEUE_TASK_DIR".to_string(), self.paths.task_dir(&task.id.to_string()).display().to_string()),
                ("POWERQUEUE_BRANCH".to_string(), branch.to_string()),
                ("POWERQUEUE_WORKTREE".to_string(), worktree.display().to_string()),
                ("POWERQUEUE_DATA_DIR".to_string(), self.paths.data_dir.display().to_string()),
                ("POWERQUEUE_STATE_DIR".to_string(), self.paths.state_dir.display().to_string()),
            ];
            let output = run_commands(worktree, &self.cfg.repo.setup, &env).context("repo.setup commands")?;
            self.log(Some(task.id), None, EventLevel::Debug, "worktree.setup", "setup commands finished", serde_json::json!({ "output": output.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect::<String>() }));
        }
        // A reused branch keeps the base recorded when it was created.
        let base = match &start {
            Some(s) => Some((s.reference.clone(), s.sha.clone())),
            None => self.store.last_task_event(task.id, BRANCH_CREATED_EVENT)?.and_then(|e| e.branch_base()),
        };
        self.log(
            Some(task.id),
            None,
            EventLevel::Info,
            "worktree.ready",
            &format!(
                "worktree {} on {} branch {branch}{}",
                worktree.display(),
                if new_branch { "new" } else { "existing" },
                base.as_ref().map(|(r, sha)| format!(" from {r} at {}", short_sha(sha))).unwrap_or_default()
            ),
            serde_json::json!({
                "path": worktree,
                "branch": branch,
                "new_branch": new_branch,
                "base": base.as_ref().map(|(r, _)| r),
                "base_sha": base.as_ref().map(|(_, sha)| sha),
                "stale_base": new_branch && fetch_error.is_some(),
            }),
        );
        Ok(fetched)
    }
}

/// First 12 characters of a SHA, for messages.
fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
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
    use crate::scheduler::pick_next;

    fn github_daemon(dir: &Path, endpoint: &str) -> Daemon {
        let paths = Paths::rooted(dir);
        let secrets = Secrets::with_backend(Box::new(crate::secrets::FileBackend::new(paths.secrets_file())));
        secrets.set(SecretKind::GitHubToken, "test-token").unwrap();
        let mut cfg = Config::default();
        cfg.linear.enabled = false;
        cfg.github.enabled = true;
        cfg.github.repository = "acme/app".into();
        cfg.github.endpoint = endpoint.into();
        Daemon::new(cfg, paths, Store::open_in_memory().unwrap(), secrets).unwrap()
    }

    #[tokio::test]
    async fn github_poll_is_independent_and_respects_offline_disabled_and_rate_limits() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = github_daemon(dir.path(), &server.uri());
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "600"))
            .expect(1)
            .mount(&server)
            .await;
        daemon.offline = true;
        daemon.poll_github(Utc::now()).await.unwrap();
        daemon.offline = false;
        daemon.cfg.github.enabled = false;
        daemon.poll_github(Utc::now()).await.unwrap();
        assert!(server.received_requests().await.unwrap().is_empty());
        daemon.cfg.github.enabled = true;
        daemon.rt.force_sync = true;
        daemon.poll_linear(Utc::now()).await.unwrap();
        assert!(daemon.rt.force_sync, "Linear must not consume the shared sync request");
        daemon.poll_github(Utc::now()).await.unwrap();
        assert!(daemon.rt.github.next_allowed.unwrap() > Utc::now() + Duration::seconds(590));
        daemon.poll_github(Utc::now()).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "force sync must respect GitHub rate limits");
    }

    #[tokio::test]
    async fn github_completion_dispatch_respects_closing_and_comment_settings() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = github_daemon(dir.path(), &server.uri());
        let task = Task::new(
            "acme/app#1",
            "Fix bug",
            TaskSource::GitHub { repository: "acme/app".into(), number: 1, url: "https://github.com/acme/app/issues/1".into() },
        );
        daemon.store.insert_task(&task).unwrap();
        Mock::given(method("POST"))
            .and(path("/repos/acme/app/issues/1/comments"))
            .and(body_partial_json(serde_json::json!({"body": "Done"})))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        daemon.update_issue(&task, LinearTarget::Done, Some("Done".into())).await;
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "closing defaults off");
        daemon.cfg.github.post_comments = false;
        daemon.cfg.github.close_on_complete = true;
        Mock::given(method("PATCH"))
            .and(path("/repos/acme/app/issues/1"))
            .and(body_partial_json(serde_json::json!({"state": "closed"})))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        daemon.update_issue(&task, LinearTarget::Done, Some("Done again".into())).await;
        assert_eq!(daemon.store.count_events_of_kind("github.closed", Utc::now() - Duration::minutes(1)).unwrap(), 1);
        daemon.offline = true;
        daemon.update_issue(&task, LinearTarget::Done, Some("offline".into())).await;
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn only_fresh_assistant_output_recovers_attention_after_replay() {
        use crate::domain::{Task, TaskSource};
        use crate::secrets::FileBackend;
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let store = Store::open_in_memory().unwrap();
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
        let mut daemon = Daemon::new(Config::default(), paths, store.clone(), secrets).unwrap();
        let mut task = Task::new("ATTN-1", "Waiting", TaskSource::Manual);
        task.state = TaskState::NeedsAttention;
        task.last_error = Some("waiting for permission: Bash".into());
        store.insert_task(&task).unwrap();
        let at = Utc::now() - Duration::minutes(1);
        let path = dir.path().join("transcript.jsonl");
        let mut session = Session {
            id: uuid::Uuid::new_v4(),
            task_id: task.id,
            attempt: 1,
            model: ModelTier::sonnet(),
            state: SessionState::Idle,
            tmux_session: "pq".into(),
            tmux_window: "@1".into(),
            pane_id: None,
            pid: None,
            transcript_path: Some(path.display().to_string()),
            exit_code: None,
            started_at: at - Duration::minutes(1),
            ended_at: None,
            last_activity_at: at,
            error: None,
            agent_session_id: None,
            waiting_since: None,
            waited_secs: 0,
        };
        store.insert_session(&session).unwrap();
        let mut transcript = std::fs::File::create(&path).unwrap();
        for (kind, timestamp) in [("assistant", at - Duration::seconds(1)), ("system", at + Duration::seconds(1))] {
            writeln!(transcript, "{}", serde_json::json!({"type": kind, "timestamp": timestamp})).unwrap();
        }
        daemon.tail_transcripts(Utc::now()).await.unwrap();
        assert_eq!(store.get_task(task.id).unwrap().unwrap().state, TaskState::NeedsAttention);
        daemon.rt.nudged.insert(session.id);
        writeln!(transcript, "{}", serde_json::json!({"type": "assistant", "timestamp": at + Duration::seconds(2)})).unwrap();
        daemon.tail_transcripts(Utc::now()).await.unwrap();
        let recovered = store.get_task(task.id).unwrap().unwrap();
        assert_eq!(recovered.state, TaskState::Running);
        assert_eq!(recovered.last_error, None);
        assert_eq!(store.get_session(session.id).unwrap().unwrap().state, SessionState::Running);
        assert!(!daemon.rt.nudged.contains(&session.id));
        assert_eq!(store.count_events_of_kind("task.attention_resolved", at).unwrap(), 1);

        // A later blocker must survive a daemon restart that rereads old output.
        store.update_task(&task).unwrap();
        session.last_activity_at = at + Duration::seconds(3);
        store.update_session(&session).unwrap();
        daemon.rt.readers.clear();
        daemon.tail_transcripts(Utc::now()).await.unwrap();
        assert_eq!(store.get_task(task.id).unwrap().unwrap().state, TaskState::NeedsAttention);
    }

    #[tokio::test]
    async fn rescore_refreshes_reasons_when_only_the_model_changes() {
        use crate::domain::{Criticality, Task, TaskSource};
        use crate::secrets::FileBackend;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let store = Store::open_in_memory().unwrap();
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
        let mut cfg = Config::default();
        cfg.priority.live_reload = false;
        let rules_path = cfg.priority_file(&paths);
        std::fs::create_dir_all(rules_path.parent().unwrap()).unwrap();
        std::fs::write(&rules_path, "## High\n- source: manual\n\n## Models\n- high: opus\n").unwrap();
        let mut daemon = Daemon::new(cfg, paths, store.clone(), secrets).unwrap();
        let mut task = Task::new("LBL-1", "Grouped", TaskSource::Manual);
        task.labels = vec!["model/fable".into()];
        store.insert_task(&task).unwrap();
        let since = Utc::now() - Duration::minutes(1);

        daemon.refresh_rules(Utc::now()).await.unwrap();
        let stored = store.get_task(task.id).unwrap().unwrap();
        assert!(stored.score_reasons.contains(&"model opus from ## Models".to_string()), "{:?}", stored.score_reasons);

        // Same criticality and score; only the model row changes.
        std::fs::write(&rules_path, "## High\n- source: manual\n\n## Models\n- if label: model/fable: fable\n- high: opus\n")
            .unwrap();
        daemon.rt.rules_loaded = false;
        daemon.refresh_rules(Utc::now()).await.unwrap();
        let stored = store.get_task(task.id).unwrap().unwrap();
        assert_eq!(stored.criticality, Criticality::High);
        assert!(
            stored.score_reasons.contains(&"model fable from ## Models line 5 (if label: model/fable)".to_string()),
            "{:?}",
            stored.score_reasons
        );
        assert_eq!(store.count_events_of_kind("task.models_changed", since).unwrap(), 2);
    }

    /// A throttled task whose agent waits in-session for its usage reset
    /// is not re-scored: its unsatisfied blocker must not move it to
    /// `blocked` under the live session (nothing would probe the window).
    #[tokio::test]
    async fn rescore_skips_a_throttled_task_with_a_live_session() {
        use crate::domain::{LinkedIssue, Task, TaskSource};
        use crate::secrets::FileBackend;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let store = Store::open_in_memory().unwrap();
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
        let mut cfg = Config::default();
        cfg.priority.live_reload = false;
        let mut daemon = Daemon::new(cfg, paths, store.clone(), secrets).unwrap();
        let now = Utc::now();
        let blocker = LinkedIssue { key: "DEP-1".into(), title: String::new(), state_type: "started".into(), pr_merged: false };
        let mut waiting = Task::new("THR-1", "waiting in-session", TaskSource::Manual);
        waiting.state = TaskState::Throttled;
        waiting.attempts = 1;
        waiting.blocked_by = vec![blocker.clone()];
        store.insert_task(&waiting).unwrap();
        let mut idle = Task::new("THR-2", "no session", TaskSource::Manual);
        idle.state = TaskState::Throttled;
        idle.blocked_by = vec![blocker];
        store.insert_task(&idle).unwrap();
        store
            .insert_session(&Session {
                id: uuid::Uuid::new_v4(),
                task_id: waiting.id,
                attempt: 1,
                model: ModelTier::sonnet(),
                state: SessionState::Running,
                tmux_session: "pq".into(),
                tmux_window: "@none".into(),
                pane_id: None,
                pid: None,
                transcript_path: None,
                exit_code: None,
                started_at: now - Duration::minutes(1),
                ended_at: None,
                last_activity_at: now,
                error: None,
                agent_session_id: None,
                waiting_since: None,
                waited_secs: 0,
            })
            .unwrap();

        daemon.refresh_rules(now).await.unwrap();
        assert_eq!(store.get_task(waiting.id).unwrap().unwrap().state, TaskState::Throttled, "left to its session");
        assert_eq!(store.get_task(idle.id).unwrap().unwrap().state, TaskState::Blocked, "no session: blocked as usual");
    }

    /// Switches of the mock Linear used by the dependency tests.
    #[derive(Default)]
    struct MockState {
        /// DEP-B is Done.
        b_done: AtomicBool,
        /// DEP-C2 (second child of DEP-P) is Done.
        c2_done: AtomicBool,
        /// DEP-B's GitHub PR is merged.
        b_merged: AtomicBool,
        /// Attachment lookups fail.
        attachments_fail: AtomicBool,
        /// DEP-A sits in Backlog: open, but not in the queued list.
        a_backlog: AtomicBool,
        /// `parent_status` lookups served.
        parent_checks: std::sync::atomic::AtomicUsize,
        mutations: std::sync::Mutex<Vec<serde_json::Value>>,
    }

    /// Stateful mock Linear GraphQL server: DEP-A is blocked by DEP-B,
    /// DEP-P is the parent of DEP-C1 (done) and DEP-C2.
    struct LinearMock(std::sync::Arc<MockState>);

    impl LinearMock {
        fn issue(&self, key: &str) -> serde_json::Value {
            let st = &self.0;
            let state = |closed: bool| {
                if closed {
                    serde_json::json!({ "name": "Done", "type": "completed" })
                } else {
                    serde_json::json!({ "name": "Todo", "type": "unstarted" })
                }
            };
            let linked = |key: &str, closed: bool| serde_json::json!({ "identifier": key, "title": format!("Title {key}"), "state": { "type": state(closed)["type"] } });
            let no_more = serde_json::json!({ "hasNextPage": false, "endCursor": null });
            let mut issue = serde_json::json!({
                "id": format!("uuid-{key}"), "identifier": key, "title": format!("Title {key}"), "description": "",
                "url": format!("https://linear.app/t/issue/{key}"), "priority": 2,
                "labels": { "nodes": [] }, "state": state(false), "team": { "key": "DEP" },
                "children": { "nodes": [], "pageInfo": no_more },
                "inverseRelations": { "nodes": [], "pageInfo": no_more },
                "createdAt": "2026-10-01T00:00:00.000Z", "updatedAt": "2026-10-01T00:00:00.000Z"
            });
            let b_done = st.b_done.load(Ordering::SeqCst);
            let c2_done = st.c2_done.load(Ordering::SeqCst);
            match key {
                "DEP-A" => {
                    if st.a_backlog.load(Ordering::SeqCst) {
                        issue["state"] = serde_json::json!({ "name": "Backlog", "type": "backlog" });
                    }
                    issue["inverseRelations"]["nodes"] = serde_json::json!([
                        { "type": "blocks", "issue": linked("DEP-B", b_done) },
                        { "type": "related", "issue": linked("DEP-X", false) }
                    ]);
                }
                "DEP-B" => issue["state"] = state(b_done),
                "DEP-P" => issue["children"]["nodes"] = serde_json::json!([linked("DEP-C1", true), linked("DEP-C2", c2_done)]),
                "DEP-C2" => {
                    issue["parent"] = serde_json::json!({ "identifier": "DEP-P" });
                    issue["state"] = state(c2_done);
                }
                _ => {}
            }
            issue
        }
    }

    impl wiremock::Respond for LinearMock {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let st = &self.0;
            let body: serde_json::Value = request.body_json().expect("json body");
            let query = body["query"].as_str().unwrap_or_default();
            let vars = &body["variables"];
            let key_of = |v: &serde_json::Value| v.as_str().unwrap_or_default().trim_start_matches("uuid-").to_string();
            let error = |msg: String| {
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({ "errors": [{ "message": msg }] }))
            };
            let data = if query.contains("issueUpdate") || query.contains("commentCreate") {
                st.mutations.lock().unwrap().push(body.clone());
                serde_json::json!({ "issueUpdate": { "success": true }, "commentCreate": { "success": true } })
            } else if query.contains("workflowStates") {
                serde_json::json!({ "workflowStates": { "nodes": [
                    { "id": "state-todo", "name": "Todo", "type": "unstarted", "team": { "key": "DEP" } },
                    { "id": "state-done", "name": "Done", "type": "completed", "team": { "key": "DEP" } }
                ] } })
            } else if query.contains("attachments") {
                if st.attachments_fail.load(Ordering::SeqCst) {
                    return error("attachments are down".into());
                }
                let merged = key_of(&vars["id"]) == "DEP-B" && st.b_merged.load(Ordering::SeqCst);
                let nodes = if merged {
                    serde_json::json!([{ "url": "https://github.com/o/r/pull/7", "sourceType": "github", "metadata": { "status": "merged" } }])
                } else {
                    serde_json::json!([])
                };
                serde_json::json!({ "issue": { "attachments": { "nodes": nodes } } })
            } else if query.contains("in: $ids") {
                assert!(query.contains("inverseRelations") && query.contains("children"), "{query}");
                let nodes: Vec<serde_json::Value> =
                    vars["ids"].as_array().unwrap().iter().map(|id| self.issue(&key_of(id))).collect();
                serde_json::json!({ "issues": { "nodes": nodes } })
            } else if query.contains("issues(") {
                assert!(!query.contains("children"), "the list query must not nest connections");
                let mut nodes = vec![self.issue("DEP-P")];
                if !st.a_backlog.load(Ordering::SeqCst) {
                    nodes.push(self.issue("DEP-A"));
                }
                if !st.c2_done.load(Ordering::SeqCst) {
                    nodes.push(self.issue("DEP-C2"));
                }
                serde_json::json!({ "issues": { "nodes": nodes, "pageInfo": { "hasNextPage": false, "endCursor": null } } })
            } else if query.contains("issue(id:") {
                if query.contains("children(") {
                    st.parent_checks.fetch_add(1, Ordering::SeqCst);
                }
                serde_json::json!({ "issue": self.issue(&key_of(&vars["id"])) })
            } else {
                return error(format!("unexpected query: {query}"));
            };
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": data }))
        }
    }

    /// A daemon talking to a fresh mock Linear; returns the shared mock state.
    async fn dependency_daemon(store: &Store, dir: &Path) -> (Daemon, std::sync::Arc<MockState>, wiremock::MockServer) {
        let server = wiremock::MockServer::start().await;
        let state = std::sync::Arc::new(MockState::default());
        wiremock::Mock::given(wiremock::matchers::method("POST")).respond_with(LinearMock(state.clone())).mount(&server).await;
        let daemon = daemon_on(store, dir, &server).await;
        (daemon, state, server)
    }

    /// A daemon (fresh runtime state) on `store`, polling `server`.
    async fn daemon_on(store: &Store, dir: &Path, server: &wiremock::MockServer) -> Daemon {
        use crate::secrets::FileBackend;
        let paths = Paths::rooted(dir);
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
        secrets.set(SecretKind::LinearApiKey, "lin_api_test").unwrap();
        let mut cfg = Config::default();
        cfg.priority.live_reload = false;
        cfg.linear.endpoint = format!("{}/graphql", server.uri());
        // Never touch the developer's tmux server or a real repository.
        cfg.tmux.socket_name = Some(format!("powerqueue-test-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
        cfg.repo.path = dir.join("repo").display().to_string();
        Daemon::new(cfg, paths, store.clone(), secrets).unwrap()
    }

    /// One forced Linear poll followed by a re-score.
    async fn sync_now(daemon: &mut Daemon, now: DateTime<Utc>) {
        daemon.rt.force_sync = true;
        daemon.poll_linear(now).await.unwrap();
        daemon.refresh_rules(now).await.unwrap();
    }

    #[tokio::test]
    async fn pause_stops_launches_and_resume_restores_them() {
        use crate::domain::{Criticality, Task, TaskSource};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let (mut daemon, _mock, _server) = dependency_daemon(&store, dir.path()).await;
        let now = Utc::now();
        let mut task = Task::new("PAUSE-1", "queued work", TaskSource::Manual);
        task.criticality = Criticality::Critical;
        store.insert_task(&task).unwrap();

        daemon.apply_command(&DaemonCommand::PauseScheduling { reason: Some("budget review".into()) }, now).await.unwrap();
        let pause = store.scheduling_pause().unwrap().expect("pause recorded");
        assert_eq!(pause.reason.as_deref(), Some("budget review"));
        assert_eq!(store.count_events_of_kind("daemon.paused", now - Duration::minutes(1)).unwrap(), 1);
        // A second pause command keeps the original instant (the CLI only sends one per transition).
        daemon.apply_command(&DaemonCommand::PauseScheduling { reason: None }, now + Duration::minutes(5)).await.unwrap();
        assert_eq!(store.scheduling_pause().unwrap().unwrap().since, pause.since);
        // A pause the CLI wrote directly (kv row, no command yet) holds too.
        store.kv_delete(SCHEDULING_PAUSE_KEY).unwrap();
        store.kv_set(SCHEDULING_PAUSE_KEY, &SchedulingPause { since: now, reason: None }).unwrap();

        daemon.launch_tasks(now).await.unwrap();
        let t = store.get_task_by_key("PAUSE-1").unwrap().unwrap();
        assert_eq!(t.state, TaskState::Queued, "paused: the task is not even considered");
        for kind in ["task.starting", "task.throttled", "task.error"] {
            assert_eq!(store.count_events_of_kind(kind, now - Duration::minutes(1)).unwrap(), 0, "{kind} while paused");
        }

        daemon.apply_command(&DaemonCommand::ResumeScheduling, now).await.unwrap();
        assert!(store.scheduling_pause().unwrap().is_none());
        assert_eq!(store.count_events_of_kind("daemon.resumed", now - Duration::minutes(1)).unwrap(), 1);
        daemon.apply_command(&DaemonCommand::ResumeScheduling, now).await.unwrap();
        assert!(store.scheduling_pause().unwrap().is_none(), "resume is idempotent on the switch");
        daemon.launch_tasks(now).await.unwrap();
        let t = store.get_task_by_key("PAUSE-1").unwrap().unwrap();
        assert_ne!(t.state, TaskState::Queued, "resumed: the task was considered (started or failed to start): {:?}", t.state);
    }

    /// A start that fails before the launch (here: the repository does not
    /// exist, so the worktree cannot be created) crashes its task and hands
    /// the slot to the next candidate of the same pass.
    #[tokio::test]
    async fn a_failed_start_frees_its_slot_for_the_next_candidate() {
        use crate::domain::{Criticality, Task, TaskSource};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let (mut daemon, _mock, _server) = dependency_daemon(&store, dir.path()).await;
        daemon.cfg.scheduler.max_concurrent = 1;
        let now = Utc::now();
        for (key, score) in [("SLOT-1", 2.0), ("SLOT-2", 1.0)] {
            let mut task = Task::new(key, "queued work", TaskSource::Manual);
            task.criticality = Criticality::Critical;
            task.score = score;
            store.insert_task(&task).unwrap();
        }

        daemon.launch_tasks(now).await.unwrap();

        for key in ["SLOT-1", "SLOT-2"] {
            let t = store.get_task_by_key(key).unwrap().unwrap();
            assert_eq!(t.state, TaskState::Crashed, "{key}: {:?} {:?}", t.state, t.last_error);
            assert_eq!(t.attempts, 1, "{key} was tried once");
            assert!(t.last_error.as_deref().is_some_and(|e| e.contains("worktree setup failed")), "{key}: {:?}", t.last_error);
        }
        assert!(store.list_live_sessions().unwrap().is_empty());
        assert_eq!(
            store.count_events_of_kind("task.starting", now - Duration::minutes(1)).unwrap(),
            2,
            "both candidates were started in one pass"
        );
    }

    #[tokio::test]
    async fn a_retry_ends_and_forgets_the_live_session_a_refused_one_names_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let (mut daemon, _mock, _server) = dependency_daemon(&store, dir.path()).await;
        let now = Utc::now();
        let live = |task: &Task| Session {
            id: uuid::Uuid::new_v4(),
            task_id: task.id,
            attempt: 1,
            model: ModelTier::sonnet(),
            state: SessionState::Idle,
            tmux_session: "pq".into(),
            tmux_window: "@none".into(),
            pane_id: None,
            pid: None,
            transcript_path: None,
            exit_code: None,
            started_at: now - Duration::minutes(1),
            ended_at: None,
            last_activity_at: now,
            error: None,
            agent_session_id: None,
            waiting_since: None,
            waited_secs: 0,
        };

        // Waiting on a human, nudged once: the retry ends the session, and
        // the daemon forgets it was nudged.
        let mut waiting = Task::new("RETRY-1", "blocked", TaskSource::Manual);
        waiting.state = TaskState::NeedsAttention;
        store.insert_task(&waiting).unwrap();
        let session = live(&waiting);
        store.insert_session(&session).unwrap();
        daemon.rt.nudged.insert(session.id);
        daemon.apply_command(&DaemonCommand::Retry { task_id: waiting.id }, now).await.unwrap();
        assert_eq!(store.get_task(waiting.id).unwrap().unwrap().state, TaskState::Queued);
        assert_eq!(store.get_session(session.id).unwrap().unwrap().state, SessionState::Killed);
        assert!(!daemon.rt.nudged.contains(&session.id), "the ended session is forgotten");
        let events = store.events_for_task(waiting.id, 10).unwrap();
        let retried = events.iter().find(|e| e.kind == "task.retried").expect("task.retried");
        assert_eq!(retried.session_id, Some(session.id), "the event names the session the retry ended");

        // Running: the retry is refused and its event names no session.
        let mut running = Task::new("RETRY-2", "busy", TaskSource::Manual);
        running.state = TaskState::Running;
        store.insert_task(&running).unwrap();
        let session = live(&running);
        store.insert_session(&session).unwrap();
        daemon.apply_command(&DaemonCommand::Retry { task_id: running.id }, now).await.unwrap();
        assert_eq!(store.get_session(session.id).unwrap().unwrap().state, SessionState::Idle, "untouched");
        let events = store.events_for_task(running.id, 10).unwrap();
        let ignored = events.iter().find(|e| e.kind == "task.retry_ignored").expect("task.retry_ignored");
        assert_eq!(ignored.session_id, None);
    }

    #[tokio::test]
    async fn a_ticket_created_this_tick_is_scored_before_it_is_scheduled() {
        use crate::domain::Criticality;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let (mut daemon, _mock, _server) = dependency_daemon(&store, dir.path()).await;
        // Every mock issue has Linear priority 2 (high).
        let rules_path = daemon.cfg.priority_file(&daemon.paths);
        std::fs::create_dir_all(rules_path.parent().unwrap()).unwrap();
        std::fs::write(&rules_path, "## Critical\n- priority: high\n\n## Models\n- critical: fable\n").unwrap();
        daemon.rt.force_sync = true;
        let before = Utc::now() - Duration::seconds(1);
        daemon.tick().await.unwrap();
        let c2 = store.get_task_by_key("DEP-C2").unwrap().unwrap();
        assert_eq!(c2.criticality, Criticality::Critical);
        // The first scheduling decision about it (start attempt or throttle)
        // already saw it as critical: no "task is normal" in its reasons.
        let events = store.events_for_task(c2.id, 100).unwrap();
        let decision = events.iter().find(|e| e.kind == "task.starting" || e.kind == "task.throttled").unwrap_or_else(|| {
            panic!("no scheduling decision after the tick: {:?}", events.iter().map(|e| &e.kind).collect::<Vec<_>>())
        });
        assert!(decision.timestamp >= before);
        let reasons = decision.data["reasons"].to_string();
        assert!(!reasons.contains("task is normal"), "{reasons}");
        assert!(reasons.contains("preferred by rules: fable"), "{reasons}");
    }

    #[tokio::test]
    async fn blocked_tasks_wait_and_parents_close_when_children_finish() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let (mut daemon, mock, _server) = dependency_daemon(&store, dir.path()).await;

        sync_now(&mut daemon, Utc::now()).await;
        let a = store.get_task_by_key("DEP-A").unwrap().unwrap();
        assert_eq!(a.state, TaskState::Blocked, "A is blocked by B, not queued");
        assert_eq!(a.waiting_on(), vec!["DEP-B"]);
        let p = store.get_task_by_key("DEP-P").unwrap().unwrap();
        assert_eq!(p.state, TaskState::Blocked, "a parent never runs");
        let c2 = store.get_task_by_key("DEP-C2").unwrap().unwrap();
        assert_eq!((c2.state, c2.parent.as_deref()), (TaskState::Queued, Some("DEP-P")));
        let open = store.list_open_tasks().unwrap();
        assert_eq!(pick_next(&open, Utc::now()).map(|t| t.key.as_str()), Some("DEP-C2"));
        assert!(mock.mutations.lock().unwrap().is_empty(), "nothing is closed while C2 is open");
        let watched: WatchedParents = store.kv_get(WATCHED_PARENTS_KEY).unwrap().unwrap();
        assert!(watched.watching.contains("DEP-P"));

        // B is done and the second child closes.
        mock.b_done.store(true, Ordering::SeqCst);
        mock.c2_done.store(true, Ordering::SeqCst);
        sync_now(&mut daemon, Utc::now()).await;
        let a = store.get_task_by_key("DEP-A").unwrap().unwrap();
        assert_eq!(a.state, TaskState::Queued, "B is Done: A is queued again");
        let p = store.get_task_by_key("DEP-P").unwrap().unwrap();
        assert_eq!(p.state, TaskState::Completed, "{:?}", p);
        assert!(p.summary.as_deref().unwrap_or_default().contains("DEP-C1, DEP-C2"));
        let sent = mock.mutations.lock().unwrap().clone();
        assert!(
            sent.iter().any(|m| m["query"].as_str().unwrap().contains("issueUpdate")
                && m["variables"]["id"] == "uuid-DEP-P"
                && m["variables"]["stateId"] == "state-done"),
            "{sent:?}"
        );
        let comment = sent.iter().find(|m| m["query"].as_str().unwrap().contains("commentCreate")).expect("a comment");
        let text = comment["variables"]["body"].as_str().unwrap();
        assert!(text.contains("**Done**") && text.contains("- DEP-C2 — Title DEP-C2 (completada)"), "{text}");
        let since = Utc::now() - Duration::minutes(5);
        assert_eq!(store.count_events_of_kind("linear.parent_closed", since).unwrap(), 1);
        assert_eq!(store.count_events_of_kind("task.unblocked", since).unwrap(), 1);
        let watched: WatchedParents = store.kv_get(WATCHED_PARENTS_KEY).unwrap().unwrap();
        assert!(watched.watching.is_empty(), "closed parents are no longer watched: {watched:?}");
        assert_eq!(watched.closed, vec!["DEP-P".to_string()]);

        // The mock keeps the parent open (as with `manage_states = false`):
        // another poll still does not close or comment twice.
        sync_now(&mut daemon, Utc::now()).await;
        assert_eq!(mock.mutations.lock().unwrap().len(), sent.len());
    }

    #[tokio::test]
    async fn merged_prs_stick_relations_refresh_outside_the_queue_and_parent_checks_are_throttled() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let (mut daemon, mock, server) = dependency_daemon(&store, dir.path()).await;
        let t0 = Utc::now();
        sync_now(&mut daemon, t0).await;
        assert_eq!(store.get_task_by_key("DEP-A").unwrap().unwrap().state, TaskState::Blocked);
        assert_eq!(mock.parent_checks.load(Ordering::SeqCst), 1);

        // Unforced polls inside PARENT_CHECK_INTERVAL do not look the parent up again.
        let t1 = t0 + Duration::seconds(61);
        daemon.poll_linear(t1).await.unwrap();
        assert_eq!(mock.parent_checks.load(Ordering::SeqCst), 1, "parent checks are throttled");
        let t2 = t0 + PARENT_CHECK_INTERVAL + Duration::seconds(1);
        daemon.poll_linear(t2).await.unwrap();
        assert_eq!(mock.parent_checks.load(Ordering::SeqCst), 2);

        // B's PR merges: A is queued while B is still open in Linear.
        mock.b_merged.store(true, Ordering::SeqCst);
        daemon.rt.unmerged_checked.clear();
        sync_now(&mut daemon, t2).await;
        let a = store.get_task_by_key("DEP-A").unwrap().unwrap();
        assert_eq!(a.state, TaskState::Queued);
        assert!(a.blocked_by[0].pr_merged);

        // A restarted daemon whose attachment lookups fail keeps "merged".
        mock.attachments_fail.store(true, Ordering::SeqCst);
        let mut restarted = daemon_on(&store, dir.path(), &server).await;
        sync_now(&mut restarted, t2).await;
        let a = store.get_task_by_key("DEP-A").unwrap().unwrap();
        assert_eq!(a.state, TaskState::Queued, "a failed lookup never re-blocks: {:?}", a.blocked_by);
        assert!(a.blocked_by[0].pr_merged);

        // A moves to Backlog (out of the queued list, still open) and B closes:
        // its stored relations still follow Linear.
        mock.a_backlog.store(true, Ordering::SeqCst);
        mock.b_done.store(true, Ordering::SeqCst);
        sync_now(&mut restarted, t2).await;
        let a = store.get_task_by_key("DEP-A").unwrap().unwrap();
        assert_eq!(a.blocked_by[0].state_type, "completed", "{:?}", a.blocked_by);
        assert_ne!(a.state, TaskState::Cancelled, "Backlog is open: never cancelled");

        // A container that had already run is cleaned up when it closes.
        let mut p = store.get_task_by_key("DEP-P").unwrap().unwrap();
        p.state = TaskState::Crashed;
        p.branch = Some("pq/dep-p".into());
        p.worktree_path = Some(dir.path().join("gone").display().to_string());
        store.update_task(&p).unwrap();
        mock.c2_done.store(true, Ordering::SeqCst);
        sync_now(&mut restarted, t2).await;
        let p = store.get_task_by_key("DEP-P").unwrap().unwrap();
        assert_eq!(p.state, TaskState::Completed);
        let events = store.events_for_task(p.id, 50).unwrap();
        assert!(
            events.iter().any(|e| e.kind == "cleanup.done" && e.message.contains("no worktree on disk")),
            "cleanup ran for the container: {events:?}"
        );
        assert_eq!(p.worktree_path, None, "the stale worktree path is cleared");
    }

    #[test]
    fn handle_stop_flag() {
        let h = DaemonHandle::new();
        assert!(!h.should_stop());
        h.stop();
        assert!(h.should_stop());
    }

    /// Mock Linear for the question relay: remembers posted comments (with
    /// the hidden markers stripped, as Linear may do) and serves them, plus
    /// the human ones a test adds, from `comments(...)`.
    #[derive(Default)]
    struct CommentState {
        comments: std::sync::Mutex<Vec<serde_json::Value>>,
        posted: std::sync::Mutex<Vec<String>>,
        /// `issueUpdate` state ids, in order.
        state_updates: std::sync::Mutex<Vec<String>>,
        /// Runs while a `comments(...)` request is in flight (another
        /// process writing the store meanwhile).
        on_fetch: std::sync::Mutex<Option<Box<dyn Fn() + Send>>>,
    }

    impl CommentState {
        fn add_human(&self, id: &str, body: &str, at: DateTime<Utc>) {
            self.comments.lock().unwrap().push(serde_json::json!({
                "id": id, "body": body, "createdAt": at.to_rfc3339(), "user": { "displayName": "Edgar" }
            }));
        }
    }

    struct CommentMock(std::sync::Arc<CommentState>);

    impl wiremock::Respond for CommentMock {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let body: serde_json::Value = request.body_json().expect("json body");
            let query = body["query"].as_str().unwrap_or_default();
            let vars = &body["variables"];
            let data = if query.contains("commentCreate") {
                let text = vars["body"].as_str().unwrap().to_string();
                let mut comments = self.0.comments.lock().unwrap();
                let id = format!("c-own-{}", comments.len());
                let stripped = text.replace(relay::QUESTION_MARKER, "").replace(relay::OWN_MARKER, "");
                let node = serde_json::json!({
                    "id": id, "body": stripped, "createdAt": Utc::now().to_rfc3339(), "user": { "displayName": "Edgar" }
                });
                comments.push(node.clone());
                self.0.posted.lock().unwrap().push(text);
                serde_json::json!({ "commentCreate": { "success": true, "comment": node } })
            } else if query.contains("issueUpdate") {
                self.0.state_updates.lock().unwrap().push(vars["stateId"].as_str().unwrap_or_default().to_string());
                serde_json::json!({ "issueUpdate": { "success": true } })
            } else if query.contains("workflowStates") {
                serde_json::json!({ "workflowStates": { "nodes": [
                    { "id": "state-progress", "name": "In Progress", "type": "started", "team": { "key": "DEP" } },
                    { "id": "state-blocked", "name": "Blocked", "type": "started", "team": { "key": "DEP" } }
                ] } })
            } else if query.contains("comments(") {
                if let Some(hook) = self.0.on_fetch.lock().unwrap().as_ref() {
                    hook();
                }
                let since: DateTime<Utc> = vars["since"].as_str().unwrap().parse().unwrap();
                let nodes: Vec<serde_json::Value> = self
                    .0
                    .comments
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|c| c["createdAt"].as_str().unwrap().parse::<DateTime<Utc>>().unwrap() > since)
                    .cloned()
                    .collect();
                serde_json::json!({ "issue": { "comments": { "nodes": nodes } } })
            } else if query.contains("issue(id:") {
                let issue = LinearMock(std::sync::Arc::new(MockState::default())).issue("REL-1");
                serde_json::json!({ "issue": issue })
            } else {
                return wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "errors": [{ "message": format!("unexpected query: {query}") }] }));
            };
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": data }))
        }
    }

    /// A daemon with `post_comments = "questions"` on a comment mock, and a
    /// Linear task in `state` with one session in `session_state`.
    async fn relay_fixture(
        dir: &Path,
        state: TaskState,
        session_state: SessionState,
        pane: Option<String>,
    ) -> (Daemon, Store, Task, Session, std::sync::Arc<CommentState>, wiremock::MockServer) {
        use crate::domain::TaskSource;
        let store = Store::open_in_memory().unwrap();
        let server = wiremock::MockServer::start().await;
        let mock = std::sync::Arc::new(CommentState::default());
        wiremock::Mock::given(wiremock::matchers::method("POST")).respond_with(CommentMock(mock.clone())).mount(&server).await;
        let mut daemon = daemon_on(&store, dir, &server).await;
        daemon.cfg.linear.post_comments = crate::config::PostComments::Questions;
        let source = TaskSource::Linear {
            issue_id: "uuid-REL-1".into(),
            identifier: "REL-1".into(),
            url: "https://linear.app/t/issue/REL-1".into(),
            team_key: "REL".into(),
        };
        let mut task = Task::new("REL-1", "Relay", source);
        task.state = state;
        task.branch = Some("pq/rel-1".into());
        store.insert_task(&task).unwrap();
        let started = Utc::now() - Duration::minutes(10);
        let session = Session {
            id: uuid::Uuid::new_v4(),
            task_id: task.id,
            attempt: 1,
            model: ModelTier::sonnet(),
            state: session_state,
            tmux_session: "pq".into(),
            tmux_window: "@1".into(),
            pane_id: pane,
            pid: None,
            transcript_path: None,
            exit_code: None,
            started_at: started,
            ended_at: None,
            last_activity_at: started,
            error: None,
            agent_session_id: None,
            waiting_since: None,
            waited_secs: 0,
        };
        store.insert_session(&session).unwrap();
        (daemon, store, task, session, mock, server)
    }

    #[tokio::test]
    async fn blocked_question_is_posted_once_and_a_reply_requeues_with_the_answer() {
        use crate::domain::HookEvent;
        let dir = tempfile::tempdir().unwrap();
        let (mut daemon, store, task, mut session, mock, _server) =
            relay_fixture(dir.path(), TaskState::Running, SessionState::Running, None).await;
        let stop = serde_json::json!({
            "last_assistant_message": "I checked the schema.\n\nShould the new column be nullable?\n[[POWERQUEUE:BLOCKED]]"
        });
        for _ in 0..2 {
            store.insert_hook_event(task.id, Some(session.id), HookEvent::Stop, &stop).unwrap();
            daemon.process_hooks(Utc::now()).await.unwrap();
        }
        let blocked = store.get_task(task.id).unwrap().unwrap();
        assert_eq!(blocked.state, TaskState::NeedsAttention);
        let posted = mock.posted.lock().unwrap().clone();
        assert_eq!(posted.len(), 1, "one question per session and text, no progress comment: {posted:?}");
        assert!(posted[0].starts_with("🤖 Pregunta del agente\n\n> Should the new column be nullable?"), "{}", posted[0]);
        assert!(posted[0].ends_with(relay::QUESTION_MARKER), "{}", posted[0]);
        let state: RelayState = store.kv_get(&relay_key(task.id)).unwrap().unwrap();
        assert!(state.open_question().is_some());
        assert_eq!(state.posted.len(), 1);

        // Progress comments stay quiet with "questions"; notices go out.
        daemon.update_linear(&blocked, LinearTarget::InProgress, Some("powerqueue started attempt 2".into())).await;
        assert_eq!(mock.posted.lock().unwrap().len(), 1);
        let mut t = blocked.clone();
        daemon.apply_effects(&mut t, None, vec![Effect::LinearComment { body: "PR on hold".into() }]).await;
        assert_eq!(mock.posted.lock().unwrap().len(), 2);
        assert!(mock.posted.lock().unwrap()[1].ends_with(relay::OWN_MARKER));

        // Nothing new yet: the task keeps waiting.
        daemon.relay_comments(Utc::now()).await.unwrap();
        assert_eq!(store.get_task(task.id).unwrap().unwrap().state, TaskState::NeedsAttention);

        // The session is gone; Edgar answers on Linear.
        session.state = SessionState::Exited;
        store.update_session(&session).unwrap();
        mock.add_human("c-human", "Yes, nullable.\n\nOld rows stay empty.", Utc::now() + Duration::seconds(1));

        // Never typed into a permission menu: the reply waits.
        let mut at_prompt = store.get_task(task.id).unwrap().unwrap();
        at_prompt.last_error = Some("waiting for permission: Bash".into());
        store.update_task(&at_prompt).unwrap();
        daemon.rt.last_relay_poll = None;
        daemon.relay_comments(Utc::now() + Duration::seconds(2)).await.unwrap();
        assert_eq!(store.get_task(task.id).unwrap().unwrap().state, TaskState::NeedsAttention);
        store.update_task(&blocked).unwrap();

        daemon.rt.last_relay_poll = None;
        daemon.relay_comments(Utc::now() + Duration::seconds(2)).await.unwrap();
        let queued = store.get_task(task.id).unwrap().unwrap();
        assert_eq!(queued.state, TaskState::Queued, "re-queued to resume with the answer");
        let state: RelayState = store.kv_get(&relay_key(task.id)).unwrap().unwrap();
        assert!(state.open_question().is_none());
        assert_eq!(
            state.pending_answer.as_deref(),
            Some(
                "Reply from Edgar on the Linear issue to your question: Yes, nullable. Old rows stay empty. \
                 (continue the task with this answer)"
            ),
            "own comments (even with the marker stripped) are not part of the answer"
        );
        assert_eq!(store.count_events_of_kind("relay.answer_queued", Utc::now() - Duration::hours(1)).unwrap(), 1);
    }

    #[tokio::test]
    async fn replies_and_hints_are_typed_into_the_live_session() {
        if which::which("tmux").is_err() {
            eprintln!("skipping: tmux is not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (mut daemon, store, task, session, mock, _server) =
            relay_fixture(dir.path(), TaskState::NeedsAttention, SessionState::Idle, None).await;
        let tmux = daemon.rt.tmux.clone();
        tmux.ensure_session("pq-relay", dir.path()).unwrap();
        let window = tmux.new_window("pq-relay", "agent", dir.path(), "cat", false).unwrap();
        let mut session = session;
        session.pane_id = Some(window.pane_id.clone());
        store.update_session(&session).unwrap();

        daemon.post_question(&task, session.id, "Which schema, A or B?", Some("schema unclear"), true).await;
        mock.add_human("c-1", "Use B", Utc::now() + Duration::seconds(1));
        daemon.relay_comments(Utc::now() + Duration::seconds(2)).await.unwrap();
        let running = store.get_task(task.id).unwrap().unwrap();
        assert_eq!(running.state, TaskState::Running);
        assert_eq!(store.get_session(session.id).unwrap().unwrap().state, SessionState::Running);

        mock.add_human("c-2", "Also add an index", Utc::now() + Duration::seconds(3));
        daemon.rt.last_relay_poll = None;
        daemon.relay_comments(Utc::now() + Duration::seconds(4)).await.unwrap();
        let mut pane = String::new();
        for _ in 0..20 {
            pane = tmux.capture_pane(&window.pane_id, 50).unwrap();
            if pane.contains("Also add an index") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = tmux.kill_session("pq-relay");
        assert!(pane.contains("Reply from Edgar on the Linear issue to your question: Use B"), "{pane}");
        assert!(pane.contains("New comment from Edgar on the Linear issue"), "{pane}");
        assert!(!pane.contains("Pregunta"), "the question itself is never typed back: {pane}");
        let since = Utc::now() - Duration::hours(1);
        assert_eq!(store.count_events_of_kind("relay.answer_sent", since).unwrap(), 1);
        assert_eq!(store.count_events_of_kind("relay.hint_sent", since).unwrap(), 1);
    }

    #[tokio::test]
    async fn a_turn_end_is_a_question_only_after_the_agent_ran_task_block() {
        use crate::domain::HookEvent;
        let dir = tempfile::tempdir().unwrap();
        let (mut daemon, store, task, session, mock, _server) =
            relay_fixture(dir.path(), TaskState::NeedsAttention, SessionState::Running, None).await;
        let stop = serde_json::json!({ "last_assistant_message": "Refactored the parser.\n\nIt is ready for review." });
        // A human ran `task block` (nothing recorded): no question.
        store.insert_hook_event(task.id, Some(session.id), HookEvent::Stop, &stop).unwrap();
        daemon.process_hooks(Utc::now()).await.unwrap();
        assert!(mock.posted.lock().unwrap().is_empty());

        // The agent ran it from its own session.
        let id = task.id.to_string();
        let sid = session.id.to_string();
        assert!(crate::cli::commands::task::record_agent_block(&store, &task, Some(&id), Some(&sid)).unwrap());
        store.insert_hook_event(task.id, Some(session.id), HookEvent::Stop, &stop).unwrap();
        daemon.process_hooks(Utc::now()).await.unwrap();
        let posted = mock.posted.lock().unwrap().clone();
        assert_eq!(posted.len(), 1, "{posted:?}");
        assert!(posted[0].contains("> It is ready for review."), "{}", posted[0]);
        let state: RelayState = store.kv_get(&relay_key(task.id)).unwrap().unwrap();
        assert_eq!(state.agent_blocked, None, "consumed by the question");
    }

    #[tokio::test]
    async fn the_same_question_after_a_reply_is_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let (mut daemon, store, task, session, mock, _server) =
            relay_fixture(dir.path(), TaskState::NeedsAttention, SessionState::Idle, None).await;
        daemon.post_question(&task, session.id, "Proceed?", None, true).await;
        daemon.post_question(&task, session.id, "Proceed?", None, true).await;
        assert_eq!(mock.posted.lock().unwrap().len(), 1, "idempotent while open");
        let key = relay_key(task.id);
        let mut state: RelayState = store.kv_get(&key).unwrap().unwrap();
        state.question.as_mut().unwrap().answered = true;
        store.kv_set(&key, &state).unwrap();
        daemon.post_question(&task, session.id, "Proceed?", None, true).await;
        assert_eq!(mock.posted.lock().unwrap().len(), 2, "a new question once the old one was answered");
        assert!(store.kv_get::<RelayState>(&key).unwrap().unwrap().open_question().is_some());
    }

    #[tokio::test]
    async fn a_failed_delivery_keeps_the_question_open_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        // Live session without a pane: typing fails.
        let (mut daemon, store, task, session, mock, _server) =
            relay_fixture(dir.path(), TaskState::NeedsAttention, SessionState::Idle, None).await;
        daemon.post_question(&task, session.id, "Which?", None, true).await;
        mock.add_human("c-1", "B", Utc::now() + Duration::seconds(1));
        daemon.relay_comments(Utc::now() + Duration::seconds(2)).await.unwrap();
        let state: RelayState = store.kv_get(&relay_key(task.id)).unwrap().unwrap();
        assert!(state.open_question().is_some(), "still waiting");
        assert_eq!(state.pending_answer, None, "nothing to replay into a later relaunch");
        assert_eq!(store.get_task(task.id).unwrap().unwrap().state, TaskState::NeedsAttention);
        // The reply is still unseen: the next poll reads it again.
        let since = state.seen_until.unwrap();
        assert!(since < Utc::now() + Duration::seconds(1), "{since}");
    }

    #[tokio::test]
    async fn a_task_changed_during_the_request_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (mut daemon, store, task, mut session, mock, _server) =
            relay_fixture(dir.path(), TaskState::NeedsAttention, SessionState::Idle, None).await;
        daemon.post_question(&task, session.id, "Which?", None, true).await;
        session.state = SessionState::Exited;
        store.update_session(&session).unwrap();
        mock.add_human("c-1", "B", Utc::now() + Duration::seconds(1));
        // `task complete` lands while the comments are being read.
        let (hook_store, id) = (store.clone(), task.id);
        *mock.on_fetch.lock().unwrap() = Some(Box::new(move || {
            let mut t = hook_store.get_task(id).unwrap().unwrap();
            t.state = TaskState::Completed;
            hook_store.update_task(&t).unwrap();
        }));
        daemon.relay_comments(Utc::now() + Duration::seconds(2)).await.unwrap();
        assert_eq!(store.get_task(task.id).unwrap().unwrap().state, TaskState::Completed, "not re-queued");
        let state: RelayState = store.kv_get(&relay_key(task.id)).unwrap().unwrap();
        assert_eq!(state.pending_answer, None);
    }

    #[tokio::test]
    async fn an_answer_moves_the_issue_out_of_the_blocked_state() {
        if which::which("tmux").is_err() {
            eprintln!("skipping: tmux is not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (mut daemon, store, task, mut session, mock, _server) =
            relay_fixture(dir.path(), TaskState::NeedsAttention, SessionState::Idle, None).await;
        daemon.cfg.linear.blocked_state = Some("Blocked".into());
        let tmux = daemon.rt.tmux.clone();
        tmux.ensure_session("pq-relay", dir.path()).unwrap();
        let window = tmux.new_window("pq-relay", "agent", dir.path(), "cat", false).unwrap();
        session.pane_id = Some(window.pane_id.clone());
        store.update_session(&session).unwrap();
        daemon.post_question(&task, session.id, "Which?", None, true).await;
        mock.add_human("c-1", "B", Utc::now() + Duration::seconds(1));
        daemon.relay_comments(Utc::now() + Duration::seconds(2)).await.unwrap();
        let _ = tmux.kill_session("pq-relay");
        assert_eq!(store.get_task(task.id).unwrap().unwrap().state, TaskState::Running);
        assert_eq!(*mock.state_updates.lock().unwrap(), vec!["state-progress".to_string()]);
    }
    fn git_in(cwd: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    #[tokio::test]
    async fn committed_overrides_and_rules_reload_without_a_command() {
        use crate::secrets::FileBackend;
        if which::which("git").is_err() {
            eprintln!("git not available; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        git_in(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join(REPO_CONFIG_FILE), "priority_file = \"docs/PRIORITY.md\"\n[scheduler]\nmax_concurrent = 3\n")
            .unwrap();
        std::fs::write(repo.join("docs/PRIORITY.md"), "## High\n- source: manual\n").unwrap();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-qm", "config"]);
        // A bare origin and a second clone: "upstream" is what lands there.
        let bare = dir.path().join("origin.git");
        git_in(dir.path(), &["init", "-q", "--bare", "-b", "main", &bare.to_string_lossy()]);
        git_in(&repo, &["remote", "add", "origin", &bare.to_string_lossy()]);
        git_in(&repo, &["push", "-q", "origin", "main"]);
        let other = dir.path().join("other");
        git_in(dir.path(), &["clone", "-q", &bare.to_string_lossy(), &other.to_string_lossy()]);
        let upstream = |files: &[(&str, &str)], msg: &str| {
            for (name, text) in files {
                std::fs::write(other.join(name), text).unwrap();
            }
            git_in(&other, &["add", "."]);
            git_in(&other, &["commit", "-qm", msg]);
            git_in(&other, &["push", "-q", "origin", "main"]);
        };

        let mut cfg = Config::default();
        cfg.repo.path = repo.display().to_string();
        cfg.repo.overrides_from = OverridesSource::DefaultBranch;
        cfg.repo.fetch_interval_secs = 1;
        cfg.save(&paths).unwrap();
        cfg.apply_repo_overrides(&repo).unwrap();
        assert_eq!(cfg.scheduler.max_concurrent, 3);
        assert_eq!(cfg.overrides.rev.as_deref(), Some("origin/main"));
        let store = Store::open_in_memory().unwrap();
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
        let mut daemon = Daemon::new(cfg, paths, store.clone(), secrets).unwrap();
        let since = Utc::now() - Duration::minutes(1);
        let mut now = Utc::now();

        // Merged upstream while the daemon was down: the first check's fetch
        // brings it, and it counts as a change from what was loaded.
        upstream(&[(REPO_CONFIG_FILE, "priority_file = \"docs/PRIORITY.md\"\n[scheduler]\nmax_concurrent = 4\n")], "while down");
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 4, "the first check compares against what was loaded");
        assert_eq!(store.count_events_of_kind("daemon.reloaded", since).unwrap(), 1);
        daemon.refresh_rules(now).await.unwrap();
        assert!(daemon.rt.rules_loaded);
        assert_eq!(daemon.rt.rules.rule_count(), 1);
        assert!(daemon.rt.watcher.is_none(), "committed rules are polled, not watched");

        // Only the rules change upstream: the rules reload, the config does not.
        upstream(&[("docs/PRIORITY.md", "## High\n- source: manual\n## Low\n- label: chore\n")], "rules");
        daemon.refresh_overrides(now).await.unwrap();
        assert!(daemon.rt.rules_loaded, "throttled: nothing is checked within OVERRIDES_CHECK_SECS");
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert!(!daemon.rt.rules_loaded);
        daemon.refresh_rules(now).await.unwrap();
        assert_eq!(daemon.rt.rules.rule_count(), 2);
        assert_eq!(store.count_events_of_kind("daemon.reloaded", since).unwrap(), 1, "rules only: no config reload");

        // The config changes upstream: full reload with the keys named.
        upstream(&[(REPO_CONFIG_FILE, "priority_file = \"docs/PRIORITY.md\"\n[scheduler]\nmax_concurrent = 5\n")], "concurrency");
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 5);
        assert_eq!(store.count_events_of_kind("daemon.reloaded", since).unwrap(), 2);
        let events = store.recent_events(5).unwrap();
        let reloaded = events.iter().find(|e| e.kind == "daemon.reloaded").unwrap();
        assert_eq!(reloaded.data["rev"], "origin/main");
        assert_eq!(reloaded.data["keys"], serde_json::json!(["priority.file", "scheduler.max_concurrent"]));

        // A transient failure (config.toml half-written while the change
        // lands) is reported once and retried: the change is not lost.
        let config_file = daemon.paths.config_file();
        let good_config = std::fs::read_to_string(&config_file).unwrap();
        std::fs::write(&config_file, "[scheduler\n").unwrap();
        upstream(&[(REPO_CONFIG_FILE, "priority_file = \"docs/PRIORITY.md\"\n[scheduler]\nmax_concurrent = 6\n")], "racing");
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 5);
        assert_eq!(store.count_events_of_kind("daemon.reload_failed", since).unwrap(), 1);
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(store.count_events_of_kind("daemon.reload_failed", since).unwrap(), 1, "retried quietly");
        std::fs::write(&config_file, good_config).unwrap();
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 6, "the retry applied the change nothing else touched");
        assert_eq!(store.count_events_of_kind("daemon.reloaded", since).unwrap(), 3);

        // A broken file keeps the previous configuration, reported once.
        upstream(&[(REPO_CONFIG_FILE, "[scheduler]\nmax_concurrent = 0\n")], "broken");
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 6);
        assert_eq!(store.count_events_of_kind("daemon.reload_failed", since).unwrap(), 2);

        // The main checkout never matters: a local branch with other values,
        // checked out there, is invisible.
        git_in(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join(REPO_CONFIG_FILE), "[scheduler]\nmax_concurrent = 9\n").unwrap();
        git_in(&repo, &["commit", "-qam", "feature"]);
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 6);
        assert_eq!(store.count_events_of_kind("daemon.reloaded", since).unwrap(), 3);

        // The repo's own `default_branch` does not move the watch: the
        // fingerprint follows the commit the loader read (origin/main).
        upstream(
            &[(
                REPO_CONFIG_FILE,
                "default_branch = \"feature\"\npriority_file = \"docs/PRIORITY.md\"\n[scheduler]\nmax_concurrent = 6\n",
            )],
            "override default branch",
        );
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 6);
        assert_eq!(daemon.cfg.repo.default_branch.as_deref(), Some("feature"));
        assert_eq!(daemon.cfg.overrides.rev.as_deref(), Some("origin/main"));
        assert_eq!(store.count_events_of_kind("daemon.reloaded", since).unwrap(), 4);
        upstream(
            &[(
                REPO_CONFIG_FILE,
                "default_branch = \"feature\"\npriority_file = \"docs/PRIORITY.md\"\n[scheduler]\nmax_concurrent = 7\n",
            )],
            "still on main",
        );
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 7, "a change on origin/main is still seen");
    }

    #[tokio::test]
    async fn a_git_failure_while_checking_overrides_is_reported_once_and_is_not_a_change() {
        use crate::secrets::FileBackend;
        if which::which("git").is_err() {
            eprintln!("git not available; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join(REPO_CONFIG_FILE), "[scheduler]\nmax_concurrent = 3\n").unwrap();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-qm", "config"]);
        let mut cfg = Config::default();
        cfg.repo.path = repo.display().to_string();
        cfg.repo.overrides_from = OverridesSource::DefaultBranch;
        cfg.repo.fetch_interval_secs = 0;
        cfg.save(&paths).unwrap();
        cfg.apply_repo_overrides(&repo).unwrap();
        let store = Store::open_in_memory().unwrap();
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
        let mut daemon = Daemon::new(cfg, paths, store.clone(), secrets).unwrap();
        let since = Utc::now() - Duration::minutes(1);
        let mut now = Utc::now();
        daemon.refresh_overrides(now).await.unwrap();
        assert!(daemon.rt.overrides_fingerprint.is_some());

        // git breaks (the repository is no longer one): no reload, no
        // fingerprint change, one event.
        std::fs::rename(repo.join(".git"), repo.join(".git-away")).unwrap();
        for _ in 0..3 {
            now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
            daemon.refresh_overrides(now).await.unwrap();
        }
        assert_eq!(store.count_events_of_kind("daemon.overrides_check_failed", since).unwrap(), 1);
        assert_eq!(store.count_events_of_kind("daemon.reload", since).unwrap(), 0);
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 3);

        // git is back: nothing changed, so nothing reloads; the next real
        // change still does.
        std::fs::rename(repo.join(".git-away"), repo.join(".git")).unwrap();
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(store.count_events_of_kind("daemon.reloaded", since).unwrap(), 0);
        std::fs::write(repo.join(REPO_CONFIG_FILE), "[scheduler]\nmax_concurrent = 4\n").unwrap();
        git_in(&repo, &["commit", "-qam", "more"]);
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 4);
        assert_eq!(store.count_events_of_kind("daemon.reloaded", since).unwrap(), 1);

        // A reload asked for by a command moves the baseline: the same
        // change is not applied a second time by the next check.
        std::fs::write(repo.join(REPO_CONFIG_FILE), "[scheduler]\nmax_concurrent = 5\n").unwrap();
        git_in(&repo, &["commit", "-qam", "again"]);
        daemon.reload_config().unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 5);
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(store.count_events_of_kind("daemon.reloaded", since).unwrap(), 1, "no duplicate reload");
    }

    #[tokio::test]
    async fn working_tree_overrides_reload_when_the_file_changes() {
        use crate::secrets::FileBackend;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut cfg = Config::default();
        cfg.repo.path = repo.display().to_string();
        cfg.priority.live_reload = false;
        cfg.save(&paths).unwrap();
        let store = Store::open_in_memory().unwrap();
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
        let mut daemon = Daemon::new(cfg, paths, store.clone(), secrets).unwrap();
        let since = Utc::now() - Duration::minutes(1);
        let mut now = Utc::now();
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 2);

        std::fs::write(repo.join(REPO_CONFIG_FILE), "[scheduler]\nmax_concurrent = 4\n[linear]\nexcluded_labels = ['x']\n")
            .unwrap();
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 4);
        assert_eq!(daemon.cfg.linear.excluded_labels, vec!["x".to_string()]);
        assert_eq!(daemon.cfg.overrides.rev, None);
        assert_eq!(store.count_events_of_kind("daemon.reloaded", since).unwrap(), 1);

        // Removing the file restores the global values.
        std::fs::remove_file(repo.join(REPO_CONFIG_FILE)).unwrap();
        now += Duration::seconds(OVERRIDES_CHECK_SECS + 1);
        daemon.refresh_overrides(now).await.unwrap();
        assert_eq!(daemon.cfg.scheduler.max_concurrent, 2);
        assert_eq!(store.count_events_of_kind("daemon.reloaded", since).unwrap(), 2);
    }
}
