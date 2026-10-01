//! Core domain types shared by every module.
//!
//! These are plain data: no I/O, no business rules beyond simple invariants.
//! Persistence lives in [`crate::store`], decisions in [`crate::budget`] and
//! [`crate::scheduler`].

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Unique task identifier (UUID v7, time-ordered). Shown to users as its
/// short form (first 8 hex chars) wherever a full id would be noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub uuid::Uuid);

impl TaskId {
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }
    /// First 8 characters of the hex form; what users type on the CLI.
    pub fn short(&self) -> String {
        self.0.simple().to_string()[..8].to_string()
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for TaskId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        uuid::Uuid::parse_str(s).map(Self)
    }
}

/// Where a task came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskSource {
    /// A Linear issue. `issue_id` is Linear's UUID, `identifier` the human key (`ENG-123`).
    Linear {
        issue_id: String,
        identifier: String,
        url: String,
        team_key: String,
    },
    /// Added by hand through `powerqueue add` or the dashboard.
    Manual,
}

impl TaskSource {
    pub fn kind(&self) -> &'static str {
        match self {
            TaskSource::Linear { .. } => "linear",
            TaskSource::Manual => "manual",
        }
    }
}

/// Business importance of a task, as decided by `PRIORITY.md` (and optionally Jev).
/// Ordered from most to least important so `Ord` can be used directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Criticality {
    Critical,
    High,
    Normal,
    Low,
}

impl Criticality {
    pub const ALL: [Criticality; 4] = [
        Criticality::Critical,
        Criticality::High,
        Criticality::Normal,
        Criticality::Low,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Criticality::Critical => "critical",
            Criticality::High => "high",
            Criticality::Normal => "normal",
            Criticality::Low => "low",
        }
    }

    /// Base score contribution; higher is scheduled first.
    pub fn base_score(&self) -> f64 {
        match self {
            Criticality::Critical => 1000.0,
            Criticality::High => 500.0,
            Criticality::Normal => 100.0,
            Criticality::Low => 10.0,
        }
    }
}

impl fmt::Display for Criticality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Criticality {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "critical" | "urgent" | "p0" => Ok(Criticality::Critical),
            "high" | "p1" => Ok(Criticality::High),
            "normal" | "medium" | "p2" | "default" => Ok(Criticality::Normal),
            "low" | "p3" | "p4" => Ok(Criticality::Low),
            other => Err(format!("unknown criticality `{other}` (expected critical|high|normal|low)")),
        }
    }
}

/// Claude model tiers that the scheduler can pick between.
/// `weight` orders by cost: Fable is the most capable and the scarcest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Fable,
    Opus,
    Sonnet,
    Haiku,
}

impl ModelTier {
    pub const ALL: [ModelTier; 4] = [ModelTier::Fable, ModelTier::Opus, ModelTier::Sonnet, ModelTier::Haiku];

    /// Alias accepted by `claude --model`.
    pub fn alias(&self) -> &'static str {
        match self {
            ModelTier::Fable => "fable",
            ModelTier::Opus => "opus",
            ModelTier::Sonnet => "sonnet",
            ModelTier::Haiku => "haiku",
        }
    }

    /// Next cheaper tier, if any.
    pub fn downgrade(&self) -> Option<ModelTier> {
        match self {
            ModelTier::Fable => Some(ModelTier::Opus),
            ModelTier::Opus => Some(ModelTier::Sonnet),
            ModelTier::Sonnet => Some(ModelTier::Haiku),
            ModelTier::Haiku => None,
        }
    }

    /// Map a full model id reported by Claude Code (`claude-fable-5-1`,
    /// `claude-sonnet-5-5`, ...) back to a tier. Unknown ids default to Sonnet,
    /// which keeps accounting conservative without inventing a new tier.
    pub fn from_model_id(id: &str) -> ModelTier {
        let id = id.to_ascii_lowercase();
        if id.contains("fable") || id.contains("mythos") {
            ModelTier::Fable
        } else if id.contains("opus") {
            ModelTier::Opus
        } else if id.contains("haiku") {
            ModelTier::Haiku
        } else {
            ModelTier::Sonnet
        }
    }

    /// Relative cost weight used for budget pacing when the user has not set
    /// explicit weights. Roughly tracks list price per output token.
    pub fn default_weight(&self) -> f64 {
        match self {
            ModelTier::Fable => 5.0,
            ModelTier::Opus => 3.0,
            ModelTier::Sonnet => 1.0,
            ModelTier::Haiku => 0.2,
        }
    }
}

impl Default for ModelTier {
    fn default() -> Self {
        ModelTier::Sonnet
    }
}

impl fmt::Display for ModelTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.alias())
    }
}

impl FromStr for ModelTier {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fable" | "mythos" => Ok(ModelTier::Fable),
            "opus" => Ok(ModelTier::Opus),
            "sonnet" => Ok(ModelTier::Sonnet),
            "haiku" => Ok(ModelTier::Haiku),
            other if other.starts_with("claude-") => Ok(ModelTier::from_model_id(other)),
            other => Err(format!("unknown model `{other}` (expected fable|opus|sonnet|haiku)")),
        }
    }
}

/// Lifecycle of a task. Transitions are enforced in [`TaskState::can_transition_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Waiting for a slot and budget.
    Queued,
    /// Worktree being created, session being launched.
    Starting,
    /// Claude Code session is active.
    Running,
    /// Session is alive but waiting for input (turn ended without completion signal).
    Idle,
    /// Session died unexpectedly; will be retried after backoff.
    Crashed,
    /// Budget or rate limit prevents running right now.
    Throttled,
    /// Paused by the user; not scheduled until resumed.
    Paused,
    /// Claude needs a human (asked a question, blocked by permissions, or reported a blocker).
    NeedsAttention,
    /// Finished successfully; resources released.
    Completed,
    /// Gave up after `max_attempts` or an unrecoverable error.
    Failed,
    /// Cancelled by the user or because the Linear issue was closed elsewhere.
    Cancelled,
}

impl TaskState {
    pub const ALL: [TaskState; 11] = [
        TaskState::Queued,
        TaskState::Starting,
        TaskState::Running,
        TaskState::Idle,
        TaskState::Crashed,
        TaskState::Throttled,
        TaskState::Paused,
        TaskState::NeedsAttention,
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Cancelled,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Starting => "starting",
            TaskState::Running => "running",
            TaskState::Idle => "idle",
            TaskState::Crashed => "crashed",
            TaskState::Throttled => "throttled",
            TaskState::Paused => "paused",
            TaskState::NeedsAttention => "needs_attention",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }

    /// True once the task will never run again.
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskState::Completed | TaskState::Failed | TaskState::Cancelled)
    }

    /// True while a tmux window / Claude process is expected to exist.
    pub fn has_live_session(&self) -> bool {
        matches!(self, TaskState::Starting | TaskState::Running | TaskState::Idle | TaskState::NeedsAttention)
    }

    /// True if the scheduler may pick this task up (possibly after backoff).
    pub fn is_schedulable(&self) -> bool {
        matches!(self, TaskState::Queued | TaskState::Crashed | TaskState::Throttled)
    }

    pub fn can_transition_to(&self, next: TaskState) -> bool {
        use TaskState::*;
        if *self == next {
            return true;
        }
        match self {
            Queued => matches!(next, Starting | Paused | Cancelled | Throttled),
            Starting => matches!(next, Running | Crashed | Failed | Cancelled | Queued),
            Running => matches!(next, Idle | Crashed | Throttled | NeedsAttention | Completed | Failed | Cancelled | Paused),
            Idle => matches!(next, Running | Crashed | NeedsAttention | Completed | Failed | Cancelled | Paused),
            Crashed => matches!(next, Starting | Queued | Failed | Cancelled | Paused | Throttled),
            Throttled => matches!(next, Queued | Starting | Cancelled | Paused),
            Paused => matches!(next, Queued | Cancelled),
            NeedsAttention => matches!(next, Running | Idle | Completed | Failed | Cancelled | Paused | Crashed),
            Completed | Failed | Cancelled => matches!(next, Queued),
        }
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        TaskState::ALL
            .iter()
            .copied()
            .find(|st| st.as_str() == s.trim().to_ascii_lowercase())
            .ok_or_else(|| format!("unknown task state `{s}`"))
    }
}

/// A unit of work: one Linear issue or one manual request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    /// Human key: Linear identifier (`ENG-123`) or `manual-<short>`; unique and
    /// used for branch, worktree and tmux window names.
    pub key: String,
    pub title: String,
    pub description: String,
    pub source: TaskSource,
    pub state: TaskState,
    pub criticality: Criticality,
    /// Scheduling score; higher first. Derived from criticality, rules, Jev and age.
    pub score: f64,
    pub labels: Vec<String>,
    /// Linear priority 0..=4 (0 = none, 1 = urgent) if known.
    pub linear_priority: Option<u8>,
    /// Linear estimate (points) if set.
    pub estimate: Option<f64>,
    /// Linear project name if any.
    pub project: Option<String>,
    /// Model forced by the user or `PRIORITY.md`; `None` lets the budget policy choose.
    pub model_override: Option<ModelTier>,
    /// Model actually used by the latest session.
    pub model: Option<ModelTier>,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub attempts: u32,
    pub max_attempts: Option<u32>,
    /// Earliest time the scheduler should consider this task (backoff / throttling).
    pub not_before: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    /// Summary provided by Claude on completion.
    pub summary: Option<String>,
    /// Which rules fired; kept for `powerqueue task explain`.
    pub score_reasons: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl Task {
    /// Minimal constructor for a new, queued task.
    pub fn new(key: impl Into<String>, title: impl Into<String>, source: TaskSource) -> Self {
        let now = Utc::now();
        Self {
            id: TaskId::new(),
            key: key.into(),
            title: title.into(),
            description: String::new(),
            source,
            state: TaskState::Queued,
            criticality: Criticality::Normal,
            score: Criticality::Normal.base_score(),
            labels: Vec::new(),
            linear_priority: None,
            estimate: None,
            project: None,
            model_override: None,
            model: None,
            worktree_path: None,
            branch: None,
            attempts: 0,
            max_attempts: None,
            not_before: None,
            last_error: None,
            summary: None,
            score_reasons: Vec::new(),
            created_at: now,
            updated_at: now,
            started_at: None,
            completed_at: None,
        }
    }

    pub fn linear_identifier(&self) -> Option<&str> {
        match &self.source {
            TaskSource::Linear { identifier, .. } => Some(identifier),
            TaskSource::Manual => None,
        }
    }

    pub fn linear_issue_id(&self) -> Option<&str> {
        match &self.source {
            TaskSource::Linear { issue_id, .. } => Some(issue_id),
            TaskSource::Manual => None,
        }
    }

    /// Safe, short identifier for branch names, tmux windows and directories.
    pub fn slug(&self) -> String {
        slugify(&self.key)
    }
}

/// Lower-case, `[a-z0-9-]` only, trimmed, max 48 chars.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = false;
    for ch in input.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    trimmed.chars().take(48).collect::<String>().trim_end_matches('-').to_string()
}

/// State of one Claude Code session (one attempt of a task).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Launching,
    Running,
    Idle,
    Exited,
    Crashed,
    Killed,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Launching => "launching",
            SessionState::Running => "running",
            SessionState::Idle => "idle",
            SessionState::Exited => "exited",
            SessionState::Crashed => "crashed",
            SessionState::Killed => "killed",
        }
    }
    pub fn is_live(&self) -> bool {
        matches!(self, SessionState::Launching | SessionState::Running | SessionState::Idle)
    }
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "launching" => Ok(SessionState::Launching),
            "running" => Ok(SessionState::Running),
            "idle" => Ok(SessionState::Idle),
            "exited" => Ok(SessionState::Exited),
            "crashed" => Ok(SessionState::Crashed),
            "killed" => Ok(SessionState::Killed),
            other => Err(format!("unknown session state `{other}`")),
        }
    }
}

/// One attempt at a task: a Claude Code process inside a tmux window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Claude Code session id (UUID) – passed as `--session-id` and reused for `--resume`.
    pub id: uuid::Uuid,
    pub task_id: TaskId,
    pub attempt: u32,
    pub model: ModelTier,
    pub state: SessionState,
    pub tmux_session: String,
    pub tmux_window: String,
    pub pane_id: Option<String>,
    pub pid: Option<u32>,
    pub transcript_path: Option<String>,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// Last time we saw activity (hook event or transcript growth).
    pub last_activity_at: DateTime<Utc>,
    pub error: Option<String>,
}

/// Token usage attributed to one session and model, summed over the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }

    /// "Weighted" tokens: an approximation of what subscription usage limits
    /// count. Cache reads are cheap (10%), cache writes cost more than plain
    /// input (125%), output is the most expensive (5x input for Claude 5 tiers).
    pub fn weighted(&self) -> f64 {
        self.input_tokens as f64
            + self.cache_creation_input_tokens as f64 * 1.25
            + self.cache_read_input_tokens as f64 * 0.10
            + self.output_tokens as f64 * 5.0
    }

    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }
}

impl std::ops::Add for TokenUsage {
    type Output = TokenUsage;
    fn add(mut self, rhs: TokenUsage) -> TokenUsage {
        TokenUsage::add(&mut self, &rhs);
        self
    }
}

/// One API call's usage as parsed from a Claude Code transcript line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub session_id: uuid::Uuid,
    pub task_id: TaskId,
    /// Claude API message id (`msg_...`); used to deduplicate transcript lines.
    pub message_id: String,
    pub model_id: String,
    pub tier: ModelTier,
    pub usage: TokenUsage,
    pub timestamp: DateTime<Utc>,
}

/// A point-in-time sample of CPU / memory for a session's process tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceSample {
    pub session_id: uuid::Uuid,
    pub task_id: TaskId,
    pub timestamp: DateTime<Utc>,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub process_count: u32,
}

/// Severity of a timeline event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl EventLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventLevel::Debug => "debug",
            EventLevel::Info => "info",
            EventLevel::Warn => "warn",
            EventLevel::Error => "error",
        }
    }
}

impl FromStr for EventLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "debug" => Ok(EventLevel::Debug),
            "info" => Ok(EventLevel::Info),
            "warn" | "warning" => Ok(EventLevel::Warn),
            "error" => Ok(EventLevel::Error),
            other => Err(format!("unknown level `{other}`")),
        }
    }
}

/// An entry in a task's timeline (persisted; shown by `task show` and the dashboard).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub task_id: Option<TaskId>,
    pub session_id: Option<uuid::Uuid>,
    pub timestamp: DateTime<Utc>,
    pub level: EventLevel,
    /// Machine-readable kind: `task.created`, `session.launched`, `hook.stop`, ...
    pub kind: String,
    pub message: String,
    /// Arbitrary structured payload (hook input, exit codes, ...).
    pub data: serde_json::Value,
}

/// Commands queued by CLI/dashboard for the running daemon (consumed in order).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonCommand {
    Pause { task_id: TaskId },
    Resume { task_id: TaskId },
    Cancel { task_id: TaskId },
    Retry { task_id: TaskId },
    /// Force a model for the next attempt.
    SetModel { task_id: TaskId, model: Option<ModelTier> },
    /// Re-read Linear immediately.
    SyncNow,
    /// Reload config + PRIORITY.md.
    Reload,
    /// Stop the daemon gracefully (sessions keep running in tmux).
    Shutdown,
}

/// Hook events Claude Code sends to `powerqueue hook`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    Stop,
    StopFailure,
    Notification,
    PreCompact,
    UserPromptSubmit,
}

impl HookEvent {
    pub const ALL: [HookEvent; 7] = [
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::Stop,
        HookEvent::StopFailure,
        HookEvent::Notification,
        HookEvent::PreCompact,
        HookEvent::UserPromptSubmit,
    ];
    pub fn as_str(&self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
            HookEvent::Notification => "Notification",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
        }
    }
}

impl FromStr for HookEvent {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        HookEvent::ALL
            .iter()
            .copied()
            .find(|h| h.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| format!("unknown hook event `{s}`"))
    }
}

/// Marker Claude is instructed to print in its final message when a task is done.
pub const DONE_MARKER: &str = "[[POWERQUEUE:DONE]]";
/// Marker Claude is instructed to print when it is blocked and needs a human.
pub const BLOCKED_MARKER: &str = "[[POWERQUEUE:BLOCKED]]";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn criticality_orders_most_important_first() {
        assert!(Criticality::Critical < Criticality::High);
        assert!(Criticality::High < Criticality::Normal);
        assert!(Criticality::Normal < Criticality::Low);
        assert_eq!("urgent".parse::<Criticality>().unwrap(), Criticality::Critical);
    }

    #[test]
    fn model_tier_round_trips() {
        for tier in ModelTier::ALL {
            assert_eq!(tier.alias().parse::<ModelTier>().unwrap(), tier);
        }
        assert_eq!(ModelTier::from_model_id("claude-fable-5-1"), ModelTier::Fable);
        assert_eq!(ModelTier::from_model_id("claude-opus-5-5"), ModelTier::Opus);
        assert_eq!(ModelTier::from_model_id("claude-haiku-4-5-20251001"), ModelTier::Haiku);
        assert_eq!(ModelTier::from_model_id("claude-sonnet-5-5"), ModelTier::Sonnet);
        assert_eq!(ModelTier::Fable.downgrade(), Some(ModelTier::Opus));
        assert_eq!(ModelTier::Haiku.downgrade(), None);
    }

    #[test]
    fn task_state_transitions() {
        use TaskState::*;
        assert!(Queued.can_transition_to(Starting));
        assert!(Running.can_transition_to(Completed));
        assert!(Crashed.can_transition_to(Starting));
        assert!(!Completed.can_transition_to(Running));
        assert!(Completed.can_transition_to(Queued)); // retry
        assert!(!Paused.can_transition_to(Running));
        for st in TaskState::ALL {
            assert_eq!(st.as_str().parse::<TaskState>().unwrap(), st);
        }
    }

    #[test]
    fn slugify_is_safe() {
        assert_eq!(slugify("ENG-123"), "eng-123");
        assert_eq!(slugify("  Fix: the thing!! "), "fix-the-thing");
        assert_eq!(slugify("a".repeat(100).as_str()).len(), 48);
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn weighted_usage_prefers_output() {
        let u = TokenUsage { input_tokens: 100, output_tokens: 100, cache_creation_input_tokens: 0, cache_read_input_tokens: 1000 };
        assert_eq!(u.total(), 1200);
        assert!((u.weighted() - (100.0 + 500.0 + 100.0)).abs() < 1e-9);
    }

    #[test]
    fn task_id_short_form() {
        let id = TaskId::new();
        assert_eq!(id.short().len(), 8);
        assert_eq!(id.to_string().parse::<TaskId>().unwrap(), id);
    }
}
