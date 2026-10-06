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
    Linear { issue_id: String, identifier: String, url: String, team_key: String },
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
    pub const ALL: [Criticality; 4] = [Criticality::Critical, Criticality::High, Criticality::Normal, Criticality::Low];

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

/// A coding-agent CLI that can run tasks and has its own subscription budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Claude Code (`claude`).
    Claude,
    /// OpenAI Codex CLI (`codex`).
    Codex,
    /// Google Antigravity CLI (`agy`), the CLI behind Google AI Pro/Ultra.
    Gemini,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Claude, Provider::Codex, Provider::Gemini];

    /// Canonical lower-case name used in config keys, CLI flags and kv keys.
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Gemini => "gemini",
        }
    }

    /// Human name of the product behind the provider.
    pub fn display_name(&self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex CLI",
            Provider::Gemini => "Antigravity CLI",
        }
    }

    /// The model unknown ids of this provider are accounted as: a mid-range
    /// model so pacing stays conservative without inventing a new one.
    pub fn default_model(&self) -> ModelTier {
        match self {
            Provider::Claude => ModelTier::sonnet(),
            Provider::Codex => ModelTier::new("gpt-6-astra"),
            Provider::Gemini => ModelTier::new("gemini-3-pro"),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = String;
    /// Accepts the canonical names plus `antigravity` / `agy` for `gemini`
    /// and `claude-code` / `codex-cli` spellings.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "anthropic" => Ok(Provider::Claude),
            "codex" | "codex-cli" | "openai" => Ok(Provider::Codex),
            "gemini" | "antigravity" | "agy" | "google" => Ok(Provider::Gemini),
            other => {
                Err(format!("unknown provider `{other}` (expected claude|codex|gemini; `antigravity`/`agy` also mean gemini)"))
            }
        }
    }
}

/// A model the scheduler can pick: the canonical alias the provider's CLI
/// accepts (`fable`, `opus`, `sonnet`, `haiku`, `gpt-6.1-sol`, `gemini-3-pro`,
/// ...). String-backed so users can add models in config without a release.
///
/// The provider is inferred from the name (see [`ModelTier::provider`]); a
/// name that does not follow the known patterns can be given with an explicit
/// `provider:name` prefix (`codex:custom-slug`), which is kept in the
/// canonical form. Serialises as a plain string. Orders alphabetically, which
/// is only used for stable map keys; capability order lives in
/// `budget.providers.<p>.models.<m>.rank`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModelTier(String);

/// Claude aliases that stand on their own.
const CLAUDE_ALIASES: [&str; 5] = ["fable", "mythos", "opus", "sonnet", "haiku"];

impl ModelTier {
    /// Normalise `name`: trim, lower-case, map Claude ids (`claude-opus-5-5`)
    /// and `mythos` to their alias, and drop a redundant `provider:` prefix
    /// when the bare name already infers that provider. Never fails; use
    /// [`FromStr`] to reject names no provider claims.
    pub fn new(name: &str) -> ModelTier {
        let lower = name.trim().to_ascii_lowercase();
        if let Some((prefix, rest)) = lower.split_once(':')
            && let Ok(p) = prefix.parse::<Provider>()
        {
            let rest = rest.trim();
            let canonical = if p == Provider::Claude { claude_alias(rest).unwrap_or(rest.to_string()) } else { rest.to_string() };
            return if infer_provider(&canonical) == Some(p) {
                ModelTier(canonical)
            } else {
                ModelTier(format!("{}:{canonical}", p.as_str()))
            };
        }
        if let Some(alias) = claude_alias(&lower) {
            return ModelTier(alias);
        }
        if lower.starts_with("claude-")
            && let Some(alias) = claude_alias_in(&lower)
        {
            return ModelTier(alias);
        }
        ModelTier(lower)
    }

    pub fn fable() -> ModelTier {
        ModelTier("fable".to_string())
    }
    pub fn opus() -> ModelTier {
        ModelTier("opus".to_string())
    }
    pub fn sonnet() -> ModelTier {
        ModelTier("sonnet".to_string())
    }
    pub fn haiku() -> ModelTier {
        ModelTier("haiku".to_string())
    }

    /// Canonical form: the alias, or `provider:alias` for names whose provider
    /// cannot be inferred. This is what is serialised and displayed.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The model name passed to the provider's CLI (`--model <alias>`): the
    /// canonical form without any `provider:` prefix.
    pub fn alias(&self) -> &str {
        match self.0.split_once(':') {
            Some((prefix, rest)) if prefix.parse::<Provider>().is_ok() => rest,
            _ => &self.0,
        }
    }

    /// Which CLI runs this model. Inferred from the name: `fable`, `mythos`,
    /// `opus`, `sonnet`, `haiku` and `claude-*` are Claude; `gpt-*`, `o1*`,
    /// `o3*`, `o4*` and `codex*` are Codex; `gemini-*` is Gemini; an explicit
    /// `provider:` prefix wins. A bare name no rule matches (which
    /// [`FromStr`] rejects, but [`ModelTier::new`] and deserialisation allow
    /// for old rows) is attributed to Claude.
    pub fn provider(&self) -> Provider {
        self.known_provider().unwrap_or(Provider::Claude)
    }

    /// The provider when the name follows a known pattern or carries an
    /// explicit prefix; `None` for bare names no provider claims.
    pub fn known_provider(&self) -> Option<Provider> {
        if let Some((prefix, _)) = self.0.split_once(':')
            && let Ok(p) = prefix.parse::<Provider>()
        {
            return Some(p);
        }
        infer_provider(&self.0)
    }

    /// True when the name can be attributed to a provider (see [`ModelTier::known_provider`]).
    pub fn is_known(&self) -> bool {
        self.known_provider().is_some()
    }

    /// Map a full model id reported by a CLI back to a tier. Claude ids
    /// (`claude-fable-5-1`, `claude-sonnet-5-5`, ...) map to their alias as
    /// before, and anything that does not look like a Claude id or another
    /// provider's model defaults to Sonnet, which keeps accounting
    /// conservative without inventing a new tier. Ids of other providers
    /// (`gpt-6.1-sol`, `gemini-3-pro`, `codex:custom`) pass through.
    pub fn from_model_id(id: &str) -> ModelTier {
        let lower = id.trim().to_ascii_lowercase();
        if let Some(alias) = claude_alias_in(&lower) {
            return ModelTier(alias);
        }
        let candidate = ModelTier::new(&lower);
        match candidate.known_provider() {
            Some(Provider::Claude) | None => ModelTier::sonnet(),
            Some(_) => candidate,
        }
    }

    /// Like [`ModelTier::from_model_id`] but for a known provider: ids that
    /// contain a Claude alias map to it for Claude; otherwise the id is used
    /// as is when it infers `provider`, else `provider.default_model()`.
    pub fn from_model_id_for(provider: Provider, id: &str) -> ModelTier {
        let lower = id.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return provider.default_model();
        }
        if provider == Provider::Claude {
            return ModelTier::from_model_id(&lower);
        }
        let candidate = ModelTier::new(&lower);
        if candidate.known_provider() == Some(provider) { candidate } else { provider.default_model() }
    }
}

/// The Claude alias for a bare name (`mythos` is an alias of `fable`).
fn claude_alias(name: &str) -> Option<String> {
    match name {
        "fable" | "mythos" => Some("fable".to_string()),
        "opus" => Some("opus".to_string()),
        "sonnet" => Some("sonnet".to_string()),
        "haiku" => Some("haiku".to_string()),
        _ => None,
    }
}

/// The Claude alias contained in a full id (`claude-opus-5-5` → `opus`);
/// `claude-*` ids without a known family default to Sonnet.
fn claude_alias_in(id: &str) -> Option<String> {
    if id.contains("fable") || id.contains("mythos") {
        Some("fable".to_string())
    } else if id.contains("opus") {
        Some("opus".to_string())
    } else if id.contains("haiku") {
        Some("haiku".to_string())
    } else if id.contains("sonnet") || id.starts_with("claude-") {
        Some("sonnet".to_string())
    } else {
        None
    }
}

/// Provider inferred from a bare, lower-cased name.
fn infer_provider(name: &str) -> Option<Provider> {
    if CLAUDE_ALIASES.contains(&name) || name.starts_with("claude-") {
        Some(Provider::Claude)
    } else if name.starts_with("gpt-")
        || name.starts_with("o1")
        || name.starts_with("o3")
        || name.starts_with("o4")
        || name.starts_with("codex")
    {
        Some(Provider::Codex)
    } else if name.starts_with("gemini-") {
        Some(Provider::Gemini)
    } else {
        None
    }
}

impl Default for ModelTier {
    /// Sonnet: the model unknown Claude usage is attributed to.
    fn default() -> Self {
        ModelTier::sonnet()
    }
}

impl fmt::Display for ModelTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ModelTier {
    type Err = String;
    /// Strict parse for user input: bare names must follow a known pattern or
    /// use the explicit `provider:name` form.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err("empty model name".to_string());
        }
        if let Some((prefix, rest)) = trimmed.split_once(':') {
            if prefix.trim().parse::<Provider>().is_err() {
                return Err(format!(
                    "unknown provider `{}` in `{trimmed}` (expected claude:|codex:|gemini: before the model name)",
                    prefix.trim()
                ));
            }
            if rest.trim().is_empty() {
                return Err(format!("`{trimmed}` has no model name after the provider"));
            }
            return Ok(ModelTier::new(trimmed));
        }
        let tier = ModelTier::new(trimmed);
        if tier.is_known() {
            Ok(tier)
        } else {
            Err(format!(
                "unknown model `{}` (expected fable|opus|sonnet|haiku or claude-*, gpt-*/o1*/o3*/o4*/codex* for Codex, gemini-* for Gemini, or an explicit `codex:<name>` / `gemini:<name>` / `claude:<name>`)",
                tier.as_str()
            ))
        }
    }
}

impl Serialize for ModelTier {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ModelTier {
    /// Lenient: any string is accepted (normalised through [`ModelTier::new`])
    /// so rows written by older versions keep loading.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(ModelTier::new(&s))
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
            Throttled => matches!(next, Queued | Starting | Running | Cancelled | Paused),
            Paused => matches!(next, Queued | Cancelled),
            NeedsAttention => matches!(next, Queued | Running | Idle | Completed | Failed | Cancelled | Paused | Crashed),
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
    /// Cycle status word of the Linear issue (`active`, `next`, `past`,
    /// `future`); `None` for manual tasks and issues outside any cycle.
    #[serde(default)]
    pub cycle: Option<String>,
    /// Number of the issue's cycle (Linear's `Cycle.number`), if any.
    #[serde(default)]
    pub cycle_number: Option<u32>,
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
            cycle: None,
            cycle_number: None,
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

/// One attempt at a task: an agent CLI process inside a tmux window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// powerqueue's session id (UUID). For Claude Code it is passed as
    /// `--session-id` and reused for `--resume`; other CLIs get their own id
    /// in `agent_session_id`.
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
    /// The CLI's own session id when it generates one (Codex thread uuid,
    /// Antigravity conversation id); `None` for Claude Code, where `id` is
    /// passed as `--session-id`. Discovered after launch, used for resume.
    #[serde(default)]
    pub agent_session_id: Option<String>,
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
    /// API message id (`msg_...` for Claude, `<thread>-<ordinal>` for Codex);
    /// used to deduplicate transcript lines.
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
    Pause {
        task_id: TaskId,
    },
    Resume {
        task_id: TaskId,
    },
    Cancel {
        task_id: TaskId,
    },
    Retry {
        task_id: TaskId,
    },
    /// Force a model for the next attempt.
    SetModel {
        task_id: TaskId,
        model: Option<ModelTier>,
    },
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
    PostToolUse,
    PostToolUseFailure,
}

impl HookEvent {
    pub const ALL: [HookEvent; 9] = [
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::Stop,
        HookEvent::StopFailure,
        HookEvent::Notification,
        HookEvent::PreCompact,
        HookEvent::UserPromptSubmit,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
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
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
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
        for tier in [ModelTier::fable(), ModelTier::opus(), ModelTier::sonnet(), ModelTier::haiku()] {
            assert_eq!(tier.alias().parse::<ModelTier>().unwrap(), tier);
            assert_eq!(tier.provider(), Provider::Claude);
            assert_eq!(tier.as_str(), tier.alias());
            let json = serde_json::to_string(&tier).unwrap();
            assert_eq!(json, format!("\"{}\"", tier.alias()));
            assert_eq!(serde_json::from_str::<ModelTier>(&json).unwrap(), tier);
        }
        assert_eq!("Mythos".parse::<ModelTier>().unwrap(), ModelTier::fable());
        assert_eq!(" OPUS ".parse::<ModelTier>().unwrap(), ModelTier::opus());
        assert_eq!(ModelTier::from_model_id("claude-fable-5-1"), ModelTier::fable());
        assert_eq!(ModelTier::from_model_id("claude-opus-5-5"), ModelTier::opus());
        assert_eq!(ModelTier::from_model_id("claude-haiku-4-5-20251001"), ModelTier::haiku());
        assert_eq!(ModelTier::from_model_id("claude-sonnet-5-5"), ModelTier::sonnet());
        assert_eq!(ModelTier::from_model_id("claude-unknown-9"), ModelTier::sonnet());
        assert_eq!(ModelTier::from_model_id(""), ModelTier::sonnet(), "unknown ids stay on sonnet as before");
        assert_eq!(ModelTier::from_model_id("gpt-6.1-sol"), ModelTier::new("gpt-6.1-sol"));
        assert_eq!(ModelTier::from_model_id("gemini-3-flash").provider(), Provider::Gemini);
        assert_eq!(ModelTier::default(), ModelTier::sonnet());
    }

    #[test]
    fn model_tier_infers_providers() {
        let gpt: ModelTier = "gpt-6.1-sol".parse().unwrap();
        assert_eq!(gpt.provider(), Provider::Codex);
        assert_eq!(gpt.alias(), "gpt-6.1-sol");
        assert_eq!("o3-pro".parse::<ModelTier>().unwrap().provider(), Provider::Codex);
        assert_eq!("codex-mini".parse::<ModelTier>().unwrap().provider(), Provider::Codex);
        assert_eq!("gemini-3-pro".parse::<ModelTier>().unwrap().provider(), Provider::Gemini);
        assert_eq!("claude-opus-5-5".parse::<ModelTier>().unwrap(), ModelTier::opus());

        let custom: ModelTier = "codex:custom".parse().unwrap();
        assert_eq!(custom.provider(), Provider::Codex);
        assert_eq!(custom.alias(), "custom");
        assert_eq!(custom.as_str(), "codex:custom");
        assert_eq!(custom.to_string(), "codex:custom");
        assert_eq!(serde_json::from_str::<ModelTier>("\"codex:custom\"").unwrap(), custom);
        // Redundant prefixes are dropped; aliases of the prefix are accepted.
        assert_eq!("claude:opus".parse::<ModelTier>().unwrap(), ModelTier::opus());
        assert_eq!("agy:gemini-3-pro".parse::<ModelTier>().unwrap().as_str(), "gemini-3-pro");
        assert_eq!("antigravity:flash".parse::<ModelTier>().unwrap().as_str(), "gemini:flash");
        // A prefix that contradicts the name is honoured as written.
        let odd: ModelTier = "codex:opus".parse().unwrap();
        assert_eq!(odd.provider(), Provider::Codex);
        assert_eq!(odd.alias(), "opus");

        assert_eq!(ModelTier::from_model_id_for(Provider::Codex, "gpt-6-luna").as_str(), "gpt-6-luna");
        assert_eq!(ModelTier::from_model_id_for(Provider::Codex, "mystery"), ModelTier::new("gpt-6-astra"));
        assert_eq!(ModelTier::from_model_id_for(Provider::Codex, ""), Provider::Codex.default_model());
        assert_eq!(ModelTier::from_model_id_for(Provider::Gemini, "gemini-3-flash").as_str(), "gemini-3-flash");
        assert_eq!(ModelTier::from_model_id_for(Provider::Gemini, "gpt-6-luna"), ModelTier::new("gemini-3-pro"));
        assert_eq!(ModelTier::from_model_id_for(Provider::Claude, "claude-opus-5-5"), ModelTier::opus());
        assert_eq!(ModelTier::from_model_id_for(Provider::Claude, "nope"), ModelTier::sonnet());
    }

    #[test]
    fn model_tier_rejects_unknown_bare_names_with_help() {
        let err = "turbo".parse::<ModelTier>().unwrap_err();
        assert!(err.contains("unknown model `turbo`"), "{err}");
        assert!(err.contains("codex:<name>"), "{err}");
        assert!("".parse::<ModelTier>().is_err());
        assert!("codex:".parse::<ModelTier>().is_err());
        let err = "bogus:thing".parse::<ModelTier>().unwrap_err();
        assert!(err.contains("unknown provider `bogus`"), "{err}");
        // Lenient paths still accept them (old rows), attributed to Claude.
        let lenient = ModelTier::new("turbo");
        assert!(!lenient.is_known());
        assert_eq!(lenient.provider(), Provider::Claude);
        assert_eq!(serde_json::from_str::<ModelTier>("\"turbo\"").unwrap(), lenient);
    }

    #[test]
    fn provider_parsing_and_names() {
        for p in Provider::ALL {
            assert_eq!(p.as_str().parse::<Provider>().unwrap(), p);
            assert_eq!(p.to_string(), p.as_str());
            assert_eq!(p.default_model().provider(), p);
            assert_eq!(serde_json::to_string(&p).unwrap(), format!("\"{}\"", p.as_str()));
        }
        assert_eq!("antigravity".parse::<Provider>().unwrap(), Provider::Gemini);
        assert_eq!("AGY".parse::<Provider>().unwrap(), Provider::Gemini);
        assert!("bard".parse::<Provider>().unwrap_err().contains("expected claude|codex|gemini"));
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
        let u =
            TokenUsage { input_tokens: 100, output_tokens: 100, cache_creation_input_tokens: 0, cache_read_input_tokens: 1000 };
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
