//! Dashboard state (no terminal I/O; testable).

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Serialize;

use crate::budget::{Ledger, Ledgers, Policy, Prediction, RATE_LIMITS_KEY, RateLimitState};
use crate::cli::output::{human_bytes, human_duration, human_f64};
use crate::config::Config;
use crate::domain::{Criticality, Event, ModelTier, Provider, ResourceSample, Session, Task, TaskId, TaskSource, TaskState};
use crate::store::Store;

/// Everything the UI needs for one frame. Serialises to JSON for
/// `dashboard --once --json`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub taken_at: Option<DateTime<Utc>>,
    pub daemon_alive: bool,
    pub daemon_pid: Option<u32>,
    pub tasks: Vec<Task>,
    pub sessions: Vec<Session>,
    /// One ledger per enabled provider; `None` when they could not be loaded.
    pub ledgers: Option<Ledgers>,
    /// Claude's ledger, duplicated from `ledgers` so JSON readers of earlier
    /// versions keep working. New code reads `ledgers`.
    pub ledger: Option<Ledger>,
    /// Enabled providers in `budget.provider_order` (the order of the blocks).
    pub provider_order: Vec<Provider>,
    /// Until when a provider is on cooldown (rate-limit marks or an
    /// observation that says the allowance is exhausted).
    pub cooldowns: BTreeMap<Provider, DateTime<Utc>>,
    /// The model the policy would give a normal-criticality, default-cost
    /// task right now (`None` = throttled or nothing enabled).
    pub next_model: Option<ModelTier>,
    pub recent_events: Vec<Event>,
    pub max_concurrent: u32,
    /// Weighted tokens per task.
    pub weighted_tokens: BTreeMap<TaskId, f64>,
    /// Latest resource sample of each task's live session.
    pub resources: BTreeMap<TaskId, ResourceSample>,
    /// Per-model share of its provider's period budget (from config, every
    /// enabled provider), for gauge labels.
    pub shares: BTreeMap<ModelTier, f64>,
    pub tmux_session: String,
}

impl Snapshot {
    /// Whether a task newly needs attention compared with the previous refresh.
    /// Pure comparison; unchanged blockers do not trigger repeated alerts.
    pub fn has_new_attention_since(&self, previous: &Self) -> bool {
        self.tasks.iter().any(|task| {
            task.state == TaskState::NeedsAttention
                && !previous.tasks.iter().any(|old| old.id == task.id && old.state == TaskState::NeedsAttention)
        })
    }

    /// Read everything from the store. The ledger is optional: when it cannot
    /// be computed the budget panel says so instead of failing the frame.
    pub fn load(store: &Store, cfg: &Config, now: DateTime<Utc>) -> Result<Snapshot> {
        let tasks = store.list_tasks()?;
        let sessions = store.list_sessions()?;
        let mut weighted_tokens = BTreeMap::new();
        let mut resources = BTreeMap::new();
        for task in &tasks {
            if task.state.is_terminal() && task.attempts == 0 {
                continue;
            }
            let usage = store.usage_for_task(task.id)?;
            if !usage.is_zero() {
                weighted_tokens.insert(task.id, usage.weighted());
            }
            if task.state.has_live_session()
                && let Some(session) = sessions.iter().filter(|s| s.task_id == task.id).max_by_key(|s| s.attempt)
                && session.state.is_live()
                && let Some(sample) = store.latest_resource_sample(session.id)?
            {
                resources.insert(task.id, sample);
            }
        }
        let ledgers = match Ledgers::load(store, &cfg.budget, now) {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "cannot load budget ledgers for the dashboard");
                None
            }
        };
        let mut limits: RateLimitState = store.kv_get(RATE_LIMITS_KEY).ok().flatten().unwrap_or_default();
        limits.clear_expired(now);
        let (cooldowns, next_model) = match &ledgers {
            Some(l) => (provider_cooldowns(cfg, l, &limits, now), next_model(cfg, l, &limits)),
            None => (BTreeMap::new(), None),
        };
        let ledger = ledgers.as_ref().and_then(|l| l.get(Provider::Claude).cloned());
        let max_age = Duration::seconds(3 * cfg.scheduler.tick_secs.max(1) as i64);
        let daemon_alive = store.daemon_alive(max_age)?;
        let daemon_pid = store.daemon_heartbeat()?.map(|(pid, _)| pid);
        Ok(Snapshot {
            taken_at: Some(now),
            daemon_alive,
            daemon_pid,
            tasks,
            sessions,
            ledgers,
            ledger,
            provider_order: cfg.budget.enabled_providers_in_order(),
            cooldowns,
            next_model,
            recent_events: store.recent_events(100)?,
            max_concurrent: cfg.scheduler.max_concurrent,
            weighted_tokens,
            resources,
            shares: model_shares(cfg),
            tmux_session: cfg.tmux.session_name.clone(),
        })
    }

    /// Ledgers in `provider_order` (the order the budget panel draws them).
    pub fn ordered_ledgers(&self) -> Vec<&Ledger> {
        self.ledgers.as_ref().map(|l| l.ordered(&self.provider_order)).unwrap_or_default()
    }

    /// Number of tasks currently holding a slot.
    pub fn slots_used(&self) -> usize {
        self.tasks.iter().filter(|t| t.state.has_live_session()).count()
    }

    /// Latest session of a task, if any.
    pub fn latest_session(&self, task_id: TaskId) -> Option<&Session> {
        self.sessions.iter().filter(|s| s.task_id == task_id).max_by_key(|s| s.attempt)
    }

    /// Last `n` events of a task, oldest first.
    pub fn events_for(&self, task_id: TaskId, n: usize) -> Vec<&Event> {
        let mut v: Vec<&Event> = self
            .recent_events
            .iter()
            .filter(|e| e.task_id == Some(task_id) && e.level != crate::domain::EventLevel::Debug)
            .collect();
        if v.len() > n {
            v.drain(..v.len() - n);
        }
        v
    }
}

/// Share of every enabled model of every enabled provider, keyed by model
/// (model names are unique across providers).
pub fn model_shares(cfg: &Config) -> BTreeMap<ModelTier, f64> {
    cfg.budget
        .providers
        .iter()
        .filter(|(_, pb)| pb.enabled)
        .flat_map(|(_, pb)| pb.models.iter().filter(|(_, m)| m.enabled).map(|(t, m)| (t.clone(), m.share)))
        .collect()
}

/// What the policy would run now for a normal-criticality, default-cost task
/// (the same question `budget show` asks); `None` when nothing is eligible.
pub fn next_model(cfg: &Config, ledgers: &Ledgers, limits: &RateLimitState) -> Option<ModelTier> {
    let mut probe = Task::new("probe", "probe", TaskSource::Manual);
    probe.criticality = Criticality::Normal;
    Policy::new(&cfg.budget, ledgers, limits).decide(&probe, Prediction::default_guess(), &[]).model
}

/// Until when each provider is cooling down: every enabled model
/// rate-limited, or its observation says the allowance is exhausted (the
/// later instant wins). Providers that are not on cooldown are absent.
pub fn provider_cooldowns(
    cfg: &Config,
    ledgers: &Ledgers,
    limits: &RateLimitState,
    now: DateTime<Utc>,
) -> BTreeMap<Provider, DateTime<Utc>> {
    ledgers
        .iter()
        .filter_map(|(p, ledger)| {
            let marks = limits.provider_blocked_until(&cfg.budget, p, now);
            let observed = ledger.observed.as_ref().and_then(|o| o.cooldown_until()).filter(|u| *u > now);
            let until = match (marks, observed) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            until.map(|u| (p, u))
        })
        .collect()
}

/// One row of the task table, already formatted.
#[derive(Debug, Clone, PartialEq)]
pub struct RowData {
    pub task_id: TaskId,
    pub key: String,
    pub state: TaskState,
    pub criticality: Criticality,
    pub model: Option<ModelTier>,
    pub tokens: String,
    pub cpu: String,
    pub rss: String,
    pub age: String,
    /// Keys the task waits on (blockers / open sub-issues), `-` when none.
    pub waiting: String,
    pub title: String,
}

/// Rows for the table: live tasks first, then by score; terminal tasks are
/// hidden when `hide_terminal`.
pub fn rows_for(snapshot: &Snapshot, hide_terminal: bool, now: DateTime<Utc>) -> Vec<RowData> {
    let mut tasks: Vec<Task> = snapshot.tasks.iter().filter(|t| !hide_terminal || !t.state.is_terminal()).cloned().collect();
    crate::cli::commands::status::sort_for_display(&mut tasks);
    tasks
        .into_iter()
        .map(|t| {
            let sample = snapshot.resources.get(&t.id);
            let age = if t.state.has_live_session() && t.started_at.is_some() {
                human_duration((now - t.started_at.unwrap_or(now)).num_seconds())
            } else {
                human_duration((now - t.created_at).num_seconds())
            };
            RowData {
                task_id: t.id,
                key: t.key.clone(),
                state: t.state,
                criticality: t.criticality,
                model: t.model.clone().or(t.model_override.clone()),
                tokens: snapshot.weighted_tokens.get(&t.id).map(|w| human_f64(*w)).unwrap_or_else(|| "-".into()),
                cpu: sample.map(|s| format!("{:.0}%", s.cpu_percent)).unwrap_or_else(|| "-".into()),
                rss: sample.map(|s| human_bytes(s.rss_bytes)).unwrap_or_else(|| "-".into()),
                age,
                waiting: crate::cli::commands::status::waiting_on(&t),
                title: t.title.clone(),
            }
        })
        .collect()
}

/// Something the event loop must do on behalf of the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    Pause(TaskId),
    Resume(TaskId),
    Cancel(TaskId),
    Retry(TaskId),
    Attach(TaskId),
    Refresh,
    ToggleTerminal,
    Help,
    OpenAdd,
    /// The add-task modal was submitted with this title.
    AddTask(String),
}

/// Selection, scroll and mode state.
#[derive(Debug, Clone, Default)]
pub struct DashboardApp {
    pub snapshot: Snapshot,
    pub selected: usize,
    pub show_help: bool,
    pub status_line: Option<String>,
    /// Hide completed/failed/cancelled tasks (default on).
    pub filter_terminal: bool,
    /// Text of the add-task modal while it is open.
    pub add_input: Option<String>,
    pub now: Option<DateTime<Utc>>,
    /// Draw with ASCII symbols and borders (see [`crate::dashboard::Options`]).
    pub ascii: bool,
}

impl DashboardApp {
    /// Fresh app with terminal tasks hidden.
    pub fn new(snapshot: Snapshot) -> Self {
        Self { snapshot, filter_terminal: true, ..Default::default() }
    }

    fn now(&self) -> DateTime<Utc> {
        self.now.unwrap_or_else(Utc::now)
    }

    /// Current table rows.
    pub fn rows(&self) -> Vec<RowData> {
        rows_for(&self.snapshot, self.filter_terminal, self.now())
    }

    /// Keep the selection inside the row range.
    pub fn clamp_selection(&mut self) {
        let n = self.rows().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    /// The selected task, if any.
    pub fn selected_task(&self) -> Option<&Task> {
        let rows = self.rows();
        let id = rows.get(self.selected)?.task_id;
        self.snapshot.tasks.iter().find(|t| t.id == id)
    }

    /// Replace the snapshot, keeping the selection on the same task when possible.
    pub fn set_snapshot(&mut self, snapshot: Snapshot) {
        let keep = self.selected_task().map(|t| t.id);
        self.snapshot = snapshot;
        if let Some(id) = keep
            && let Some(pos) = self.rows().iter().position(|r| r.task_id == id)
        {
            self.selected = pos;
        }
        self.clamp_selection();
    }

    /// Translate a key press into state changes and an optional [`Action`].
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) {
            return Some(Action::Quit);
        }
        if let Some(input) = self.add_input.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.add_input = None;
                }
                KeyCode::Enter => {
                    let title = input.trim().to_string();
                    self.add_input = None;
                    if !title.is_empty() {
                        return Some(Action::AddTask(title));
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            }
            return None;
        }
        if self.show_help {
            self.show_help = false;
            return None;
        }
        self.status_line = None;
        let n = self.rows().len();
        let selected_id = self.selected_task().map(|t| t.id);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
            KeyCode::Down | KeyCode::Char('j') => {
                if n > 0 {
                    self.selected = (self.selected + 1).min(n - 1);
                }
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected = 0;
                None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = n.saturating_sub(1);
                None
            }
            KeyCode::PageDown => {
                if n > 0 {
                    self.selected = (self.selected + 10).min(n - 1);
                }
                None
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(10);
                None
            }
            KeyCode::Char('a') | KeyCode::Enter => selected_id.map(Action::Attach),
            KeyCode::Char('p') => selected_id.map(Action::Pause),
            KeyCode::Char('r') => selected_id.map(Action::Resume),
            KeyCode::Char('c') => selected_id.map(Action::Cancel),
            KeyCode::Char('R') => selected_id.map(Action::Retry),
            KeyCode::Char('n') => {
                self.add_input = Some(String::new());
                Some(Action::OpenAdd)
            }
            KeyCode::Char('t') => {
                self.filter_terminal = !self.filter_terminal;
                self.clamp_selection();
                Some(Action::ToggleTerminal)
            }
            KeyCode::Char('?') | KeyCode::Char('h') => {
                self.show_help = true;
                Some(Action::Help)
            }
            KeyCode::Char('u') | KeyCode::F(5) => Some(Action::Refresh),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskSource;

    fn task(key: &str, state: TaskState, score: f64) -> Task {
        let mut t = Task::new(key, format!("Title {key}"), TaskSource::Manual);
        t.state = state;
        t.score = score;
        t
    }

    fn snapshot() -> Snapshot {
        let mut s = Snapshot {
            tasks: vec![
                task("A", TaskState::Queued, 100.0),
                task("B", TaskState::Running, 10.0),
                task("C", TaskState::Completed, 500.0),
                task("D", TaskState::Queued, 900.0),
            ],
            max_concurrent: 2,
            ..Default::default()
        };
        s.weighted_tokens.insert(s.tasks[1].id, 12_500.0);
        s
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn attention_alerts_only_on_entry_and_rearm_after_resolution() {
        let before = snapshot();
        let mut blocked = before.clone();
        blocked.tasks[1].state = TaskState::NeedsAttention;
        assert!(blocked.has_new_attention_since(&before));
        assert!(!blocked.has_new_attention_since(&blocked));
        assert!(!before.has_new_attention_since(&blocked));
        assert!(blocked.has_new_attention_since(&before));
    }

    #[test]
    fn rows_sort_live_first_and_hide_terminal() {
        let s = snapshot();
        let rows = rows_for(&s, true, Utc::now());
        let keys: Vec<_> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["B", "D", "A"]);
        assert_eq!(rows[0].tokens, "12k");
        assert_eq!(rows[1].tokens, "-");
        assert_eq!(rows_for(&s, false, Utc::now()).len(), 4);
    }

    #[test]
    fn navigation_and_actions() {
        let mut app = DashboardApp::new(snapshot());
        assert_eq!(app.handle_key(key('j')), None);
        assert_eq!(app.selected, 1);
        assert_eq!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)), None);
        assert_eq!(app.handle_key(key('j')), None);
        assert_eq!(app.selected, 2, "clamped at the last row");
        let d = app.selected_task().unwrap().id;
        assert_eq!(app.handle_key(key('G')), None);
        assert_eq!(app.handle_key(key('g')), None);
        assert_eq!(app.selected, 0);
        let b = app.selected_task().unwrap().id;
        assert_eq!(app.handle_key(key('p')), Some(Action::Pause(b)));
        assert_eq!(app.handle_key(key('r')), Some(Action::Resume(b)));
        assert_eq!(app.handle_key(key('c')), Some(Action::Cancel(b)));
        assert_eq!(app.handle_key(key('R')), Some(Action::Retry(b)));
        assert_eq!(app.handle_key(key('a')), Some(Action::Attach(b)));
        assert_ne!(b, d);
        assert_eq!(app.handle_key(key('u')), Some(Action::Refresh));
        assert_eq!(app.handle_key(key('q')), Some(Action::Quit));
        assert_eq!(app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Some(Action::Quit));
    }

    #[test]
    fn toggle_terminal_and_help() {
        let mut app = DashboardApp::new(snapshot());
        assert_eq!(app.rows().len(), 3);
        assert_eq!(app.handle_key(key('t')), Some(Action::ToggleTerminal));
        assert_eq!(app.rows().len(), 4);
        assert_eq!(app.handle_key(key('?')), Some(Action::Help));
        assert!(app.show_help);
        assert_eq!(app.handle_key(key('q')), None, "any key closes help");
        assert!(!app.show_help);
    }

    #[test]
    fn add_modal_collects_title() {
        let mut app = DashboardApp::new(snapshot());
        assert_eq!(app.handle_key(key('n')), Some(Action::OpenAdd));
        for c in "fix it".chars() {
            assert_eq!(app.handle_key(key(c)), None);
        }
        assert_eq!(app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)), None);
        assert_eq!(app.add_input.as_deref(), Some("fix i"));
        assert_eq!(app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(Action::AddTask("fix i".into())));
        assert!(app.add_input.is_none());
        app.handle_key(key('n'));
        assert_eq!(app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), None);
        assert!(app.add_input.is_none());
    }

    #[test]
    fn set_snapshot_keeps_selection() {
        let mut app = DashboardApp::new(snapshot());
        app.handle_key(key('j'));
        let id = app.selected_task().unwrap().id;
        let mut next = app.snapshot.clone();
        next.tasks.insert(0, task("Z", TaskState::Running, 1.0));
        app.set_snapshot(next);
        assert_eq!(app.selected_task().unwrap().id, id);
        app.set_snapshot(Snapshot::default());
        assert_eq!(app.selected, 0);
        assert!(app.selected_task().is_none());
    }

    #[test]
    fn snapshot_serialises_to_json() {
        let s = snapshot();
        let json = serde_json::to_value(&s).expect("snapshot is JSON-serialisable");
        assert_eq!(json["max_concurrent"], 2);
        assert_eq!(json["tasks"].as_array().map(|t| t.len()), Some(4));
        let tokens = json["weighted_tokens"].as_object().expect("map with string keys");
        assert_eq!(tokens.len(), 1);
    }

    #[test]
    fn snapshot_helpers() {
        let s = snapshot();
        assert_eq!(s.slots_used(), 1);
        assert!(s.latest_session(s.tasks[0].id).is_none());
        assert!(s.events_for(s.tasks[0].id, 10).is_empty());
        assert!(s.ordered_ledgers().is_empty());
    }

    #[test]
    fn snapshot_loads_one_ledger_per_enabled_provider() {
        let store = Store::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.budget.providers.codex.enabled = true;
        cfg.budget.provider_order = vec![Provider::Codex, Provider::Claude];
        let now = Utc::now();
        let s = Snapshot::load(&store, &cfg, now).unwrap();
        let providers: Vec<Provider> = s.ordered_ledgers().iter().map(|l| l.provider).collect();
        assert_eq!(providers, vec![Provider::Codex, Provider::Claude]);
        assert_eq!(s.provider_order, vec![Provider::Codex, Provider::Claude]);
        assert!(s.ledger.is_some(), "claude's ledger is still exposed for JSON compatibility");
        assert!(s.shares.contains_key(&ModelTier::fable()) && s.shares.contains_key(&ModelTier::new("gpt-6.1-sol")));
        assert!(!s.shares.contains_key(&ModelTier::new("gemini-3-pro")), "gemini is disabled");
        assert!(s.cooldowns.is_empty());
        // A normal task prefers `budget.default_model` (sonnet) whatever the provider order.
        assert_eq!(s.next_model, Some(ModelTier::sonnet()));
        let json = serde_json::to_value(&s).unwrap();
        assert!(json["ledgers"]["by_provider"]["codex"].is_object());
        assert!(json["ledger"]["provider"] == "claude");
        assert_eq!(json["next_model"], "sonnet");

        // A rate-limited provider shows a cooldown and `next` moves to the other one.
        let mut limits = RateLimitState::default();
        limits.mark_provider(&cfg.budget, Provider::Claude, now + Duration::minutes(30));
        store.kv_set(RATE_LIMITS_KEY, &limits).unwrap();
        let s = Snapshot::load(&store, &cfg, now).unwrap();
        assert_eq!(s.cooldowns.get(&Provider::Claude), Some(&(now + Duration::minutes(30))));
        assert!(!s.cooldowns.contains_key(&Provider::Codex));
        assert_eq!(s.next_model.as_ref().map(|m| m.provider()), Some(Provider::Codex), "{:?}", s.next_model);
    }
}
