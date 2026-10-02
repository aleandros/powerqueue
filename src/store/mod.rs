//! SQLite persistence.
//!
//! One connection guarded by a mutex is plenty: the daemon writes a handful
//! of rows per second at most, and WAL mode lets the dashboard and CLI read
//! concurrently from their own connections. All timestamps are stored as
//! RFC 3339 UTC strings so they sort lexicographically.

use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::domain::*;

const SCHEMA: &str = include_str!("schema.sql");
/// Current schema version (`PRAGMA user_version`).
///
/// * v1: initial schema.
/// * v2: `sessions.agent_session_id`; kv `budget.calibration` renamed to
///   `budget.calibration.claude` (budgets are per provider).
pub const SCHEMA_VERSION: i64 = 2;

/// kv key of the Claude calibration before schema v2.
const LEGACY_CALIBRATION_KEY: &str = "budget.calibration";
/// kv key of the Claude calibration from schema v2 on.
const CLAUDE_CALIBRATION_KEY: &str = "budget.calibration.claude";

/// Handle to the database. Cheap to clone.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
}

/// Aggregated usage for a tier over a time range.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TierUsage {
    pub tier: ModelTier,
    pub usage: TokenUsage,
    pub messages: u64,
}

/// Per-task totals used by the estimator and the dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskUsageSummary {
    pub task_id: TaskId,
    pub criticality: Criticality,
    pub estimate: Option<f64>,
    pub labels: Vec<String>,
    pub state: TaskState,
    pub attempts: u32,
    pub usage: TokenUsage,
    pub weighted: f64,
    pub wall_secs: i64,
    pub tier: Option<ModelTier>,
}

/// Snapshot of the whole queue (dashboard / status).
#[derive(Debug, Clone, Default)]
pub struct QueueCounts {
    pub queued: usize,
    pub running: usize,
    pub idle: usize,
    pub crashed: usize,
    pub throttled: usize,
    pub paused: usize,
    pub needs_attention: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
}

fn ts(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)).map_err(|e| anyhow!("bad timestamp `{s}`: {e}"))
}

fn opt_ts(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    s.map(|v| parse_ts(&v)).transpose()
}

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

fn from_json<T: DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| anyhow!("bad json in db: {e}: {s}"))
}

impl Store {
    /// Open (and migrate) the database at `path`, creating parent directories.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self { conn: Arc::new(Mutex::new(conn)) };
        store.migrate()?;
        Ok(store)
    }

    /// In-memory database for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn: Arc::new(Mutex::new(conn)) };
        store.migrate()?;
        Ok(store)
    }

    /// Create missing tables and bring an older database up to
    /// [`SCHEMA_VERSION`]. Every step is idempotent (checked against the
    /// actual schema, not just `user_version`), so an interrupted migration
    /// is simply finished on the next open.
    fn migrate(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute_batch(SCHEMA).context("apply schema")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 2 {
            let has_column = conn
                .prepare("PRAGMA table_info(sessions)")?
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<String>>>()?
                .iter()
                .any(|c| c == "agent_session_id");
            if !has_column {
                conn.execute("ALTER TABLE sessions ADD COLUMN agent_session_id TEXT", [])
                    .context("add sessions.agent_session_id")?;
            }
            let renamed = conn.execute(
                "UPDATE OR IGNORE kv SET key = ?2 WHERE key = ?1",
                params![LEGACY_CALIBRATION_KEY, CLAUDE_CALIBRATION_KEY],
            )?;
            // If the new key already existed the update was ignored; drop the stale legacy row.
            let dropped = conn.execute("DELETE FROM kv WHERE key = ?1", params![LEGACY_CALIBRATION_KEY])?;
            tracing::info!(
                from = version,
                to = SCHEMA_VERSION,
                column_added = !has_column,
                calibration_renamed = renamed > 0,
                legacy_dropped = dropped > 0,
                "migrated database schema"
            );
        }
        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    }

    /// `PRAGMA user_version` of the open database.
    pub fn schema_version(&self) -> Result<i64> {
        let conn = self.lock();
        Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `PRAGMA integrity_check`.
    pub fn integrity_check(&self) -> Result<String> {
        let conn = self.lock();
        let s: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        Ok(s)
    }

    // ------------------------------------------------------------------ tasks

    pub fn insert_task(&self, task: &Task) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO tasks (id, key, title, description, source, source_kind, linear_issue_id, state, criticality, score,
                labels, linear_priority, estimate, project, model_override, model, worktree_path, branch, attempts, max_attempts,
                not_before, last_error, summary, score_reasons, created_at, updated_at, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)",
            params![
                task.id.to_string(),
                task.key,
                task.title,
                task.description,
                json(&task.source),
                task.source.kind(),
                task.linear_issue_id(),
                task.state.as_str(),
                task.criticality.as_str(),
                task.score,
                json(&task.labels),
                task.linear_priority,
                task.estimate,
                task.project,
                task.model_override.as_ref().map(|m| m.alias()),
                task.model.as_ref().map(|m| m.alias()),
                task.worktree_path,
                task.branch,
                task.attempts,
                task.max_attempts,
                task.not_before.as_ref().map(ts),
                task.last_error,
                task.summary,
                json(&task.score_reasons),
                ts(&task.created_at),
                ts(&task.updated_at),
                task.started_at.as_ref().map(ts),
                task.completed_at.as_ref().map(ts),
            ],
        )
        .with_context(|| format!("insert task {}", task.key))?;
        Ok(())
    }

    /// Overwrite every column of an existing task. `updated_at` is set to now.
    pub fn update_task(&self, task: &Task) -> Result<()> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE tasks SET key=?2, title=?3, description=?4, source=?5, source_kind=?6, linear_issue_id=?7, state=?8,
                criticality=?9, score=?10, labels=?11, linear_priority=?12, estimate=?13, project=?14, model_override=?15,
                model=?16, worktree_path=?17, branch=?18, attempts=?19, max_attempts=?20, not_before=?21, last_error=?22,
                summary=?23, score_reasons=?24, updated_at=?25, started_at=?26, completed_at=?27
             WHERE id=?1",
            params![
                task.id.to_string(),
                task.key,
                task.title,
                task.description,
                json(&task.source),
                task.source.kind(),
                task.linear_issue_id(),
                task.state.as_str(),
                task.criticality.as_str(),
                task.score,
                json(&task.labels),
                task.linear_priority,
                task.estimate,
                task.project,
                task.model_override.as_ref().map(|m| m.alias()),
                task.model.as_ref().map(|m| m.alias()),
                task.worktree_path,
                task.branch,
                task.attempts,
                task.max_attempts,
                task.not_before.as_ref().map(ts),
                task.last_error,
                task.summary,
                json(&task.score_reasons),
                ts(&Utc::now()),
                task.started_at.as_ref().map(ts),
                task.completed_at.as_ref().map(ts),
            ],
        )?;
        if n == 0 {
            return Err(anyhow!("task {} not found", task.id));
        }
        Ok(())
    }

    fn row_to_task(row: &Row<'_>) -> rusqlite::Result<Task> {
        let conv = |e: anyhow::Error| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into());
        let id: String = row.get("id")?;
        let source: String = row.get("source")?;
        let state: String = row.get("state")?;
        let criticality: String = row.get("criticality")?;
        let labels: String = row.get("labels")?;
        let reasons: String = row.get("score_reasons")?;
        let model_override: Option<String> = row.get("model_override")?;
        let model: Option<String> = row.get("model")?;
        let created_at: String = row.get("created_at")?;
        let updated_at: String = row.get("updated_at")?;
        Ok(Task {
            id: TaskId::from_str(&id).map_err(|e| conv(e.into()))?,
            key: row.get("key")?,
            title: row.get("title")?,
            description: row.get("description")?,
            source: from_json(&source).map_err(conv)?,
            state: TaskState::from_str(&state).map_err(|e| conv(anyhow!(e)))?,
            criticality: Criticality::from_str(&criticality).map_err(|e| conv(anyhow!(e)))?,
            score: row.get("score")?,
            labels: from_json(&labels).map_err(conv)?,
            linear_priority: row.get("linear_priority")?,
            estimate: row.get("estimate")?,
            project: row.get("project")?,
            model_override: model_override.map(|m| ModelTier::from_str(&m)).transpose().map_err(|e| conv(anyhow!(e)))?,
            model: model.map(|m| ModelTier::from_str(&m)).transpose().map_err(|e| conv(anyhow!(e)))?,
            worktree_path: row.get("worktree_path")?,
            branch: row.get("branch")?,
            attempts: row.get("attempts")?,
            max_attempts: row.get("max_attempts")?,
            not_before: opt_ts(row.get("not_before")?).map_err(conv)?,
            last_error: row.get("last_error")?,
            summary: row.get("summary")?,
            score_reasons: from_json(&reasons).map_err(conv)?,
            created_at: parse_ts(&created_at).map_err(conv)?,
            updated_at: parse_ts(&updated_at).map_err(conv)?,
            started_at: opt_ts(row.get("started_at")?).map_err(conv)?,
            completed_at: opt_ts(row.get("completed_at")?).map_err(conv)?,
        })
    }

    const TASK_COLS: &'static str =
        "id, key, title, description, source, source_kind, linear_issue_id, state, criticality, score, labels,
        linear_priority, estimate, project, model_override, model, worktree_path, branch, attempts, max_attempts, not_before,
        last_error, summary, score_reasons, created_at, updated_at, started_at, completed_at";

    pub fn get_task(&self, id: TaskId) -> Result<Option<Task>> {
        let conn = self.lock();
        let sql = format!("SELECT {} FROM tasks WHERE id = ?1", Self::TASK_COLS);
        Ok(conn.query_row(&sql, params![id.to_string()], Self::row_to_task).optional()?)
    }

    pub fn get_task_by_key(&self, key: &str) -> Result<Option<Task>> {
        let conn = self.lock();
        let sql = format!("SELECT {} FROM tasks WHERE key = ?1 COLLATE NOCASE", Self::TASK_COLS);
        Ok(conn.query_row(&sql, params![key], Self::row_to_task).optional()?)
    }

    pub fn get_task_by_linear_issue(&self, issue_id: &str) -> Result<Option<Task>> {
        let conn = self.lock();
        let sql = format!("SELECT {} FROM tasks WHERE linear_issue_id = ?1", Self::TASK_COLS);
        Ok(conn.query_row(&sql, params![issue_id], Self::row_to_task).optional()?)
    }

    /// Resolve what a user typed: full id, short id prefix, or key (case-insensitive).
    pub fn find_task(&self, needle: &str) -> Result<Option<Task>> {
        let needle = needle.trim();
        if let Ok(id) = TaskId::from_str(needle)
            && let Some(t) = self.get_task(id)?
        {
            return Ok(Some(t));
        }
        if let Some(t) = self.get_task_by_key(needle)? {
            return Ok(Some(t));
        }
        let conn = self.lock();
        let sql =
            format!("SELECT {} FROM tasks WHERE replace(id, '-', '') LIKE ?1 || '%' ORDER BY created_at DESC", Self::TASK_COLS);
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query_map(params![needle.replace('-', "").to_lowercase()], Self::row_to_task)?;
        let first = rows.next().transpose()?;
        if rows.next().is_some() {
            return Err(anyhow!("`{needle}` is ambiguous; use a longer prefix or the task key"));
        }
        Ok(first)
    }

    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let conn = self.lock();
        let sql = format!("SELECT {} FROM tasks ORDER BY created_at ASC", Self::TASK_COLS);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], Self::row_to_task)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_tasks_in_states(&self, states: &[TaskState]) -> Result<Vec<Task>> {
        Ok(self.list_tasks()?.into_iter().filter(|t| states.contains(&t.state)).collect())
    }

    /// Tasks not in a terminal state.
    pub fn list_open_tasks(&self) -> Result<Vec<Task>> {
        Ok(self.list_tasks()?.into_iter().filter(|t| !t.state.is_terminal()).collect())
    }

    pub fn delete_task(&self, id: TaskId) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM tasks WHERE id = ?1", params![id.to_string()])?;
        Ok(())
    }

    pub fn counts(&self) -> Result<QueueCounts> {
        let mut c = QueueCounts::default();
        for t in self.list_tasks()? {
            match t.state {
                TaskState::Queued | TaskState::Starting => c.queued += 1,
                TaskState::Running => c.running += 1,
                TaskState::Idle => c.idle += 1,
                TaskState::Crashed => c.crashed += 1,
                TaskState::Throttled => c.throttled += 1,
                TaskState::Paused => c.paused += 1,
                TaskState::NeedsAttention => c.needs_attention += 1,
                TaskState::Completed => c.completed += 1,
                TaskState::Failed => c.failed += 1,
                TaskState::Cancelled => c.cancelled += 1,
            }
        }
        Ok(c)
    }

    // --------------------------------------------------------------- sessions

    pub fn insert_session(&self, s: &Session) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sessions (id, task_id, attempt, model, state, tmux_session, tmux_window, pane_id, pid, transcript_path,
                exit_code, started_at, ended_at, last_activity_at, error, agent_session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                s.id.to_string(),
                s.task_id.to_string(),
                s.attempt,
                s.model.alias(),
                s.state.as_str(),
                s.tmux_session,
                s.tmux_window,
                s.pane_id,
                s.pid,
                s.transcript_path,
                s.exit_code,
                ts(&s.started_at),
                s.ended_at.as_ref().map(ts),
                ts(&s.last_activity_at),
                s.error,
                s.agent_session_id,
            ],
        )?;
        Ok(())
    }

    pub fn update_session(&self, s: &Session) -> Result<()> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE sessions SET task_id=?2, attempt=?3, model=?4, state=?5, tmux_session=?6, tmux_window=?7, pane_id=?8, pid=?9,
                transcript_path=?10, exit_code=?11, started_at=?12, ended_at=?13, last_activity_at=?14, error=?15,
                agent_session_id=?16 WHERE id=?1",
            params![
                s.id.to_string(),
                s.task_id.to_string(),
                s.attempt,
                s.model.alias(),
                s.state.as_str(),
                s.tmux_session,
                s.tmux_window,
                s.pane_id,
                s.pid,
                s.transcript_path,
                s.exit_code,
                ts(&s.started_at),
                s.ended_at.as_ref().map(ts),
                ts(&s.last_activity_at),
                s.error,
                s.agent_session_id,
            ],
        )?;
        if n == 0 {
            return Err(anyhow!("session {} not found", s.id));
        }
        Ok(())
    }

    /// Record the CLI's own session id for a session (Codex thread uuid,
    /// Antigravity conversation id). Fails when the session does not exist.
    pub fn set_agent_session_id(&self, id: uuid::Uuid, agent_session_id: &str) -> Result<()> {
        let conn = self.lock();
        let n =
            conn.execute("UPDATE sessions SET agent_session_id = ?2 WHERE id = ?1", params![id.to_string(), agent_session_id])?;
        if n == 0 {
            return Err(anyhow!("session {id} not found"));
        }
        Ok(())
    }

    fn row_to_session(row: &Row<'_>) -> rusqlite::Result<Session> {
        let conv = |e: anyhow::Error| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into());
        let id: String = row.get("id")?;
        let task_id: String = row.get("task_id")?;
        let model: String = row.get("model")?;
        let state: String = row.get("state")?;
        let started_at: String = row.get("started_at")?;
        let last_activity_at: String = row.get("last_activity_at")?;
        Ok(Session {
            id: uuid::Uuid::parse_str(&id).map_err(|e| conv(e.into()))?,
            task_id: TaskId::from_str(&task_id).map_err(|e| conv(e.into()))?,
            attempt: row.get("attempt")?,
            model: ModelTier::from_str(&model).map_err(|e| conv(anyhow!(e)))?,
            state: SessionState::from_str(&state).map_err(|e| conv(anyhow!(e)))?,
            tmux_session: row.get("tmux_session")?,
            tmux_window: row.get("tmux_window")?,
            pane_id: row.get("pane_id")?,
            pid: row.get("pid")?,
            transcript_path: row.get("transcript_path")?,
            exit_code: row.get("exit_code")?,
            started_at: parse_ts(&started_at).map_err(conv)?,
            ended_at: opt_ts(row.get("ended_at")?).map_err(conv)?,
            last_activity_at: parse_ts(&last_activity_at).map_err(conv)?,
            error: row.get("error")?,
            agent_session_id: row.get("agent_session_id")?,
        })
    }

    const SESSION_COLS: &'static str =
        "id, task_id, attempt, model, state, tmux_session, tmux_window, pane_id, pid, transcript_path,
        exit_code, started_at, ended_at, last_activity_at, error, agent_session_id";

    pub fn get_session(&self, id: uuid::Uuid) -> Result<Option<Session>> {
        let conn = self.lock();
        let sql = format!("SELECT {} FROM sessions WHERE id = ?1", Self::SESSION_COLS);
        Ok(conn.query_row(&sql, params![id.to_string()], Self::row_to_session).optional()?)
    }

    pub fn list_sessions_for_task(&self, task_id: TaskId) -> Result<Vec<Session>> {
        let conn = self.lock();
        let sql = format!("SELECT {} FROM sessions WHERE task_id = ?1 ORDER BY attempt ASC", Self::SESSION_COLS);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![task_id.to_string()], Self::row_to_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The most recent session for a task, if any.
    pub fn latest_session(&self, task_id: TaskId) -> Result<Option<Session>> {
        Ok(self.list_sessions_for_task(task_id)?.into_iter().last())
    }

    pub fn list_live_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {} FROM sessions WHERE state IN ('launching','running','idle') ORDER BY started_at ASC",
            Self::SESSION_COLS
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], Self::row_to_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Sessions that are live *or* ended at/after `since` — the set whose
    /// transcripts may still have unread usage lines.
    pub fn list_sessions_active_since(&self, since: DateTime<Utc>) -> Result<Vec<Session>> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {} FROM sessions WHERE state IN ('launching','running','idle') OR ended_at >= ?1 ORDER BY started_at ASC",
            Self::SESSION_COLS
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![ts(&since)], Self::row_to_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.lock();
        let sql = format!("SELECT {} FROM sessions ORDER BY started_at ASC", Self::SESSION_COLS);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], Self::row_to_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ------------------------------------------------------------------ usage

    /// Insert a usage record; duplicates (same message id) are ignored.
    /// Returns true if the record was new.
    pub fn record_usage(&self, u: &UsageRecord) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO usage (message_id, session_id, task_id, model_id, tier, input_tokens, output_tokens,
                cache_creation_input_tokens, cache_read_input_tokens, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                u.message_id,
                u.session_id.to_string(),
                u.task_id.to_string(),
                u.model_id,
                u.tier.alias(),
                u.usage.input_tokens as i64,
                u.usage.output_tokens as i64,
                u.usage.cache_creation_input_tokens as i64,
                u.usage.cache_read_input_tokens as i64,
                ts(&u.timestamp),
            ],
        )?;
        Ok(n > 0)
    }

    pub fn has_usage_message(&self, message_id: &str) -> Result<bool> {
        let conn = self.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM usage WHERE message_id = ?1", params![message_id], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// Usage per tier between two instants. Rows whose `tier` is not a
    /// model name any provider claims are skipped with a warning rather than
    /// being attributed to Sonnet.
    pub fn usage_by_tier(&self, since: DateTime<Utc>, until: DateTime<Utc>) -> Result<Vec<TierUsage>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT tier, SUM(input_tokens), SUM(output_tokens), SUM(cache_creation_input_tokens), SUM(cache_read_input_tokens), COUNT(*)
             FROM usage WHERE timestamp >= ?1 AND timestamp < ?2 GROUP BY tier",
        )?;
        let rows = stmt.query_map(params![ts(&since), ts(&until)], |r| {
            let tier: String = r.get(0)?;
            Ok((
                tier,
                TokenUsage {
                    input_tokens: r.get::<_, i64>(1)? as u64,
                    output_tokens: r.get::<_, i64>(2)? as u64,
                    cache_creation_input_tokens: r.get::<_, i64>(3)? as u64,
                    cache_read_input_tokens: r.get::<_, i64>(4)? as u64,
                },
                r.get::<_, i64>(5)? as u64,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (tier, usage, messages) = row?;
            match ModelTier::from_str(&tier) {
                Ok(tier) => out.push(TierUsage { tier, usage, messages }),
                Err(e) => tracing::warn!(tier = %tier, messages, error = %e, "skipping usage rows with an unknown model alias"),
            }
        }
        Ok(out)
    }

    pub fn usage_for_session(&self, session_id: uuid::Uuid) -> Result<TokenUsage> {
        let conn = self.lock();
        let u = conn.query_row(
            "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), COALESCE(SUM(cache_creation_input_tokens),0),
                    COALESCE(SUM(cache_read_input_tokens),0) FROM usage WHERE session_id = ?1",
            params![session_id.to_string()],
            |r| {
                Ok(TokenUsage {
                    input_tokens: r.get::<_, i64>(0)? as u64,
                    output_tokens: r.get::<_, i64>(1)? as u64,
                    cache_creation_input_tokens: r.get::<_, i64>(2)? as u64,
                    cache_read_input_tokens: r.get::<_, i64>(3)? as u64,
                })
            },
        )?;
        Ok(u)
    }

    pub fn usage_for_task(&self, task_id: TaskId) -> Result<TokenUsage> {
        let conn = self.lock();
        let u = conn.query_row(
            "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), COALESCE(SUM(cache_creation_input_tokens),0),
                    COALESCE(SUM(cache_read_input_tokens),0) FROM usage WHERE task_id = ?1",
            params![task_id.to_string()],
            |r| {
                Ok(TokenUsage {
                    input_tokens: r.get::<_, i64>(0)? as u64,
                    output_tokens: r.get::<_, i64>(1)? as u64,
                    cache_creation_input_tokens: r.get::<_, i64>(2)? as u64,
                    cache_read_input_tokens: r.get::<_, i64>(3)? as u64,
                })
            },
        )?;
        Ok(u)
    }

    /// Latest usage timestamp recorded for a session (activity signal).
    pub fn last_usage_at(&self, session_id: uuid::Uuid) -> Result<Option<DateTime<Utc>>> {
        let conn = self.lock();
        let s: Option<String> = conn
            .query_row("SELECT MAX(timestamp) FROM usage WHERE session_id = ?1", params![session_id.to_string()], |r| r.get(0))
            .optional()?
            .flatten();
        opt_ts(s)
    }

    /// Per-task usage summaries for every task that has a session (estimator input).
    pub fn task_usage_summaries(&self) -> Result<Vec<TaskUsageSummary>> {
        let tasks = self.list_tasks()?;
        let mut out = Vec::with_capacity(tasks.len());
        for t in tasks {
            let usage = self.usage_for_task(t.id)?;
            if usage.is_zero() && t.attempts == 0 {
                continue;
            }
            let wall_secs = match (t.started_at, t.completed_at) {
                (Some(s), Some(e)) => (e - s).num_seconds(),
                (Some(s), None) => (Utc::now() - s).num_seconds(),
                _ => 0,
            };
            out.push(TaskUsageSummary {
                task_id: t.id,
                criticality: t.criticality,
                estimate: t.estimate,
                labels: t.labels.clone(),
                state: t.state,
                attempts: t.attempts,
                weighted: usage.weighted(),
                usage,
                wall_secs,
                tier: t.model,
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------- resources

    pub fn record_resource_sample(&self, s: &ResourceSample) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO resource_samples (session_id, task_id, timestamp, cpu_percent, rss_bytes, process_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                s.session_id.to_string(),
                s.task_id.to_string(),
                ts(&s.timestamp),
                s.cpu_percent as f64,
                s.rss_bytes as i64,
                s.process_count as i64
            ],
        )?;
        Ok(())
    }

    pub fn latest_resource_sample(&self, session_id: uuid::Uuid) -> Result<Option<ResourceSample>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT session_id, task_id, timestamp, cpu_percent, rss_bytes, process_count FROM resource_samples
                 WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
                params![session_id.to_string()],
                |r| {
                    let sid: String = r.get(0)?;
                    let tid: String = r.get(1)?;
                    let t: String = r.get(2)?;
                    Ok((sid, tid, t, r.get::<_, f64>(3)? as f32, r.get::<_, i64>(4)? as u64, r.get::<_, i64>(5)? as u32))
                },
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((sid, tid, t, cpu, rss, pc)) => Ok(Some(ResourceSample {
                session_id: uuid::Uuid::parse_str(&sid)?,
                task_id: TaskId::from_str(&tid)?,
                timestamp: parse_ts(&t)?,
                cpu_percent: cpu,
                rss_bytes: rss,
                process_count: pc,
            })),
        }
    }

    /// Peak RSS and mean CPU over a session's samples.
    pub fn resource_stats(&self, session_id: uuid::Uuid) -> Result<Option<(u64, f32, u32)>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT MAX(rss_bytes), AVG(cpu_percent), COUNT(*) FROM resource_samples WHERE session_id = ?1",
                params![session_id.to_string()],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<f64>>(1)?, r.get::<_, i64>(2)?)),
            )
            .optional()?;
        Ok(match row {
            Some((Some(peak), Some(avg), n)) if n > 0 => Some((peak as u64, avg as f32, n as u32)),
            _ => None,
        })
    }

    /// Drop resource samples older than `older_than`.
    pub fn prune_resource_samples(&self, older_than: DateTime<Utc>) -> Result<usize> {
        let conn = self.lock();
        Ok(conn.execute("DELETE FROM resource_samples WHERE timestamp < ?1", params![ts(&older_than)])?)
    }

    // ----------------------------------------------------------------- events

    pub fn log_event(
        &self,
        task_id: Option<TaskId>,
        session_id: Option<uuid::Uuid>,
        level: EventLevel,
        kind: &str,
        message: &str,
        data: serde_json::Value,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO events (task_id, session_id, timestamp, level, kind, message, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                task_id.map(|t| t.to_string()),
                session_id.map(|s| s.to_string()),
                ts(&Utc::now()),
                level.as_str(),
                kind,
                message,
                data.to_string()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn row_to_event(row: &Row<'_>) -> rusqlite::Result<Event> {
        let conv = |e: anyhow::Error| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into());
        let task_id: Option<String> = row.get("task_id")?;
        let session_id: Option<String> = row.get("session_id")?;
        let timestamp: String = row.get("timestamp")?;
        let level: String = row.get("level")?;
        let data: String = row.get("data")?;
        Ok(Event {
            id: row.get("id")?,
            task_id: task_id.map(|t| TaskId::from_str(&t)).transpose().map_err(|e| conv(e.into()))?,
            session_id: session_id.map(|s| uuid::Uuid::parse_str(&s)).transpose().map_err(|e| conv(e.into()))?,
            timestamp: parse_ts(&timestamp).map_err(conv)?,
            level: EventLevel::from_str(&level).unwrap_or(EventLevel::Info),
            kind: row.get("kind")?,
            message: row.get("message")?,
            data: serde_json::from_str(&data).unwrap_or(serde_json::Value::Null),
        })
    }

    pub fn events_for_task(&self, task_id: TaskId, limit: usize) -> Result<Vec<Event>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, task_id, session_id, timestamp, level, kind, message, data FROM events
             WHERE task_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![task_id.to_string(), limit as i64], Self::row_to_event)?;
        let mut v = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        v.reverse();
        Ok(v)
    }

    pub fn recent_events(&self, limit: usize) -> Result<Vec<Event>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, task_id, session_id, timestamp, level, kind, message, data FROM events ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], Self::row_to_event)?;
        let mut v = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        v.reverse();
        Ok(v)
    }

    /// Events with id greater than `after_id` (for live tailing).
    pub fn events_after(&self, after_id: i64, limit: usize) -> Result<Vec<Event>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, task_id, session_id, timestamp, level, kind, message, data FROM events WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after_id, limit as i64], Self::row_to_event)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count_events_of_kind(&self, kind_prefix: &str, since: DateTime<Utc>) -> Result<u64> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE kind LIKE ?1 || '%' AND timestamp >= ?2",
            params![kind_prefix, ts(&since)],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    // --------------------------------------------------------------- commands

    pub fn enqueue_command(&self, cmd: &DaemonCommand) -> Result<i64> {
        let conn = self.lock();
        conn.execute("INSERT INTO commands (created_at, payload) VALUES (?1, ?2)", params![ts(&Utc::now()), json(cmd)])?;
        Ok(conn.last_insert_rowid())
    }

    /// Pop all unconsumed commands, marking them consumed.
    pub fn drain_commands(&self) -> Result<Vec<DaemonCommand>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut out = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id, payload FROM commands WHERE consumed_at IS NULL ORDER BY id ASC")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            let now = ts(&Utc::now());
            for row in rows {
                let (id, payload) = row?;
                match serde_json::from_str::<DaemonCommand>(&payload) {
                    Ok(cmd) => out.push(cmd),
                    Err(e) => tracing::warn!(id, error = %e, "dropping unparseable daemon command"),
                }
                tx.execute("UPDATE commands SET consumed_at = ?1 WHERE id = ?2", params![now, id])?;
            }
        }
        tx.commit()?;
        Ok(out)
    }

    // ------------------------------------------------------------ hook events

    pub fn insert_hook_event(
        &self,
        task_id: TaskId,
        session_id: Option<uuid::Uuid>,
        event: HookEvent,
        payload: &serde_json::Value,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO hook_events (task_id, session_id, event, payload, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![task_id.to_string(), session_id.map(|s| s.to_string()), event.as_str(), payload.to_string(), ts(&Utc::now())],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Unconsumed hook events in arrival order; marks them consumed.
    pub fn drain_hook_events(&self) -> Result<Vec<PendingHookEvent>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut out = Vec::new();
        {
            let mut stmt =
                tx.prepare("SELECT id, task_id, session_id, event, payload, created_at FROM hook_events WHERE consumed_at IS NULL ORDER BY id ASC")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?;
            let now = ts(&Utc::now());
            for row in rows {
                let (id, task_id, session_id, event, payload, created_at) = row?;
                tx.execute("UPDATE hook_events SET consumed_at = ?1 WHERE id = ?2", params![now, id])?;
                let Ok(task_id) = TaskId::from_str(&task_id) else { continue };
                let Ok(event) = HookEvent::from_str(&event) else { continue };
                out.push(PendingHookEvent {
                    id,
                    task_id,
                    session_id: session_id.and_then(|s| uuid::Uuid::parse_str(&s).ok()),
                    event,
                    payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                    created_at: parse_ts(&created_at).unwrap_or_else(|_| Utc::now()),
                });
            }
        }
        tx.commit()?;
        Ok(out)
    }

    /// Hook events for a task (consumed or not), newest last.
    pub fn hook_events_for_task(&self, task_id: TaskId, limit: usize) -> Result<Vec<PendingHookEvent>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, task_id, session_id, event, payload, created_at FROM hook_events WHERE task_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![task_id.to_string(), limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, task_id, session_id, event, payload, created_at) = row?;
            out.push(PendingHookEvent {
                id,
                task_id: TaskId::from_str(&task_id)?,
                session_id: session_id.and_then(|s| uuid::Uuid::parse_str(&s).ok()),
                event: HookEvent::from_str(&event).map_err(|e| anyhow!(e))?,
                payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                created_at: parse_ts(&created_at)?,
            });
        }
        out.reverse();
        Ok(out)
    }

    // --------------------------------------------------------------------- kv

    pub fn kv_set<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO kv (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, json(value), ts(&Utc::now())],
        )?;
        Ok(())
    }

    pub fn kv_get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let conn = self.lock();
        let v: Option<String> = conn.query_row("SELECT value FROM kv WHERE key = ?1", params![key], |r| r.get(0)).optional()?;
        v.map(|s| from_json(&s)).transpose()
    }

    pub fn kv_updated_at(&self, key: &str) -> Result<Option<DateTime<Utc>>> {
        let conn = self.lock();
        let v: Option<String> =
            conn.query_row("SELECT updated_at FROM kv WHERE key = ?1", params![key], |r| r.get(0)).optional()?;
        opt_ts(v)
    }

    pub fn kv_delete(&self, key: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM kv WHERE key = ?1", params![key])?;
        Ok(())
    }

    // ------------------------------------------------------------- jev cache

    pub fn jev_cached(&self, task_id: TaskId) -> Result<Option<JevCached>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT content_hash, score, level, confidence, raw, scored_at FROM jev_scores WHERE task_id = ?1",
                params![task_id.to_string()],
                |r| {
                    Ok(JevCached {
                        content_hash: r.get(0)?,
                        score: r.get(1)?,
                        level: r.get(2)?,
                        confidence: r.get(3)?,
                        raw: r.get(4)?,
                        scored_at: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn jev_store(&self, task_id: TaskId, c: &JevCached) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO jev_scores (task_id, content_hash, score, level, confidence, raw, scored_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(task_id) DO UPDATE SET content_hash=excluded.content_hash, score=excluded.score, level=excluded.level,
             confidence=excluded.confidence, raw=excluded.raw, scored_at=excluded.scored_at",
            params![task_id.to_string(), c.content_hash, c.score, c.level, c.confidence, c.raw, c.scored_at],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ heartbeat

    /// Record that the daemon is alive (pid + timestamp).
    pub fn heartbeat(&self, pid: u32) -> Result<()> {
        self.kv_set("daemon.heartbeat", &serde_json::json!({ "pid": pid, "at": ts(&Utc::now()) }))
    }

    /// `(pid, last heartbeat)` if a daemon has ever run.
    pub fn daemon_heartbeat(&self) -> Result<Option<(u32, DateTime<Utc>)>> {
        let v: Option<serde_json::Value> = self.kv_get("daemon.heartbeat")?;
        let Some(v) = v else { return Ok(None) };
        let pid = v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0) as u32;
        let at = v.get("at").and_then(|a| a.as_str()).map(parse_ts).transpose()?.unwrap_or_else(Utc::now);
        Ok(Some((pid, at)))
    }

    /// True if a heartbeat was written within `max_age`.
    pub fn daemon_alive(&self, max_age: Duration) -> Result<bool> {
        Ok(match self.daemon_heartbeat()? {
            Some((_, at)) => Utc::now() - at < max_age,
            None => false,
        })
    }

    // ----------------------------------------------------------------- reset

    /// Row counts of every table [`Store::reset`] would empty (`kv` included,
    /// whether or not it will be wiped), without changing anything.
    pub fn reset_counts(&self) -> Result<ResetCounts> {
        let conn = self.lock();
        let count = |table: &str| -> Result<u64> {
            let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            Ok(n as u64)
        };
        Ok(ResetCounts {
            tasks: count("tasks")?,
            sessions: count("sessions")?,
            usage: count("usage")?,
            resource_samples: count("resource_samples")?,
            events: count("events")?,
            commands: count("commands")?,
            hook_events: count("hook_events")?,
            jev_scores: count("jev_scores")?,
            kv: count("kv")?,
        })
    }

    /// Delete every row from `tasks` (sessions cascade), `sessions`, `usage`,
    /// `resource_samples`, `events`, `commands`, `hook_events` and
    /// `jev_scores` in one transaction; with `everything` also `kv` (budget
    /// calibration, cooldowns, probe results, the daemon heartbeat).
    /// Returns how many rows each table held. The schema and its version are
    /// untouched; a `VACUUM` afterwards is best effort.
    pub fn reset(&self, everything: bool) -> Result<ResetCounts> {
        let counts = self.reset_counts()?;
        {
            let mut conn = self.lock();
            let tx = conn.transaction().context("begin reset transaction")?;
            for table in ["tasks", "sessions", "usage", "resource_samples", "events", "commands", "hook_events", "jev_scores"] {
                tx.execute(&format!("DELETE FROM {table}"), []).with_context(|| format!("empty table {table}"))?;
            }
            if everything {
                tx.execute("DELETE FROM kv", []).context("empty table kv")?;
            }
            tx.commit().context("commit reset transaction")?;
        }
        if let Err(e) = self.lock().execute_batch("VACUUM") {
            tracing::warn!(error = %e, "VACUUM after reset failed (database is still consistent)");
        }
        Ok(ResetCounts { kv: if everything { counts.kv } else { 0 }, ..counts })
    }
}

/// Rows per table counted or removed by [`Store::reset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ResetCounts {
    pub tasks: u64,
    pub sessions: u64,
    pub usage: u64,
    pub resource_samples: u64,
    pub events: u64,
    pub commands: u64,
    pub hook_events: u64,
    pub jev_scores: u64,
    /// Rows in `kv`; after [`Store::reset`] this is 0 unless `everything` was set.
    pub kv: u64,
}

impl ResetCounts {
    /// Sum of every table.
    pub fn total(&self) -> u64 {
        self.tasks
            + self.sessions
            + self.usage
            + self.resource_samples
            + self.events
            + self.commands
            + self.hook_events
            + self.jev_scores
            + self.kv
    }
}

/// A hook event waiting for the daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingHookEvent {
    pub id: i64,
    pub task_id: TaskId,
    pub session_id: Option<uuid::Uuid>,
    pub event: HookEvent,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// Cached Jev scoring result.
#[derive(Debug, Clone, PartialEq)]
pub struct JevCached {
    pub content_hash: String,
    pub score: f64,
    pub level: String,
    pub confidence: f64,
    pub raw: String,
    pub scored_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear_task(key: &str) -> Task {
        Task::new(
            key,
            format!("Title {key}"),
            TaskSource::Linear {
                issue_id: format!("uuid-{key}"),
                identifier: key.to_string(),
                url: "https://linear.app/x".into(),
                team_key: "ENG".into(),
            },
        )
    }

    #[test]
    fn task_crud_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let mut t = linear_task("ENG-1");
        t.labels = vec!["bug".into()];
        t.score_reasons = vec!["label bug +10".into()];
        store.insert_task(&t).unwrap();
        let got = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(got.key, "ENG-1");
        assert_eq!(got.labels, vec!["bug"]);
        assert_eq!(got.source, t.source);

        t.state = TaskState::Running;
        t.model = Some(ModelTier::opus());
        store.update_task(&t).unwrap();
        let got = store.get_task_by_key("eng-1").unwrap().unwrap();
        assert_eq!(got.state, TaskState::Running);
        assert_eq!(got.model, Some(ModelTier::opus()));
        assert!(store.get_task_by_linear_issue("uuid-ENG-1").unwrap().is_some());

        let found = store.find_task(&t.id.short()).unwrap().unwrap();
        assert_eq!(found.id, t.id);
        assert!(store.find_task("nope").unwrap().is_none());
    }

    #[test]
    fn duplicate_key_rejected() {
        let store = Store::open_in_memory().unwrap();
        store.insert_task(&linear_task("ENG-1")).unwrap();
        assert!(store.insert_task(&linear_task("ENG-1")).is_err());
    }

    #[test]
    fn sessions_and_usage() {
        let store = Store::open_in_memory().unwrap();
        let t = linear_task("ENG-2");
        store.insert_task(&t).unwrap();
        let now = Utc::now();
        let s = Session {
            id: uuid::Uuid::new_v4(),
            task_id: t.id,
            attempt: 1,
            model: ModelTier::sonnet(),
            state: SessionState::Running,
            tmux_session: "powerqueue".into(),
            tmux_window: "eng-2".into(),
            pane_id: Some("%3".into()),
            pid: Some(123),
            transcript_path: None,
            exit_code: None,
            started_at: now,
            ended_at: None,
            last_activity_at: now,
            error: None,
            agent_session_id: None,
        };
        store.insert_session(&s).unwrap();
        assert_eq!(store.list_live_sessions().unwrap().len(), 1);
        store.set_agent_session_id(s.id, "thread-123").unwrap();
        assert_eq!(store.get_session(s.id).unwrap().unwrap().agent_session_id.as_deref(), Some("thread-123"));
        assert!(store.set_agent_session_id(uuid::Uuid::new_v4(), "x").is_err());
        let rec = UsageRecord {
            session_id: s.id,
            task_id: t.id,
            message_id: "msg_1".into(),
            model_id: "claude-sonnet-5-5".into(),
            tier: ModelTier::sonnet(),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                cache_creation_input_tokens: 30,
                cache_read_input_tokens: 40,
            },
            timestamp: now,
        };
        assert!(store.record_usage(&rec).unwrap());
        assert!(!store.record_usage(&rec).unwrap(), "duplicate message ids are ignored");
        let u = store.usage_for_session(s.id).unwrap();
        assert_eq!(u.total(), 100);
        let by_tier = store.usage_by_tier(now - Duration::hours(1), now + Duration::hours(1)).unwrap();
        assert_eq!(by_tier.len(), 1);
        assert_eq!(by_tier[0].tier, ModelTier::sonnet());
        assert_eq!(by_tier[0].messages, 1);
        // Rows with an alias no provider claims are skipped, not counted as sonnet.
        store.record_usage(&UsageRecord { message_id: "msg_odd".into(), tier: ModelTier::new("turbo"), ..rec.clone() }).unwrap();
        store
            .record_usage(&UsageRecord {
                message_id: "msg_gpt".into(),
                model_id: "gpt-6.1-sol".into(),
                tier: ModelTier::new("gpt-6.1-sol"),
                ..rec.clone()
            })
            .unwrap();
        let by_tier = store.usage_by_tier(now - Duration::hours(1), now + Duration::hours(1)).unwrap();
        assert_eq!(by_tier.len(), 2, "{by_tier:?}");
        assert!(by_tier.iter().any(|u| u.tier == ModelTier::new("gpt-6.1-sol")));
        assert!(by_tier.iter().all(|u| u.messages == 1));
        assert_eq!(store.last_usage_at(s.id).unwrap().unwrap().timestamp_millis(), now.timestamp_millis());

        let mut s2 = s.clone();
        s2.state = SessionState::Exited;
        s2.exit_code = Some(0);
        store.update_session(&s2).unwrap();
        assert_eq!(store.get_session(s.id).unwrap().unwrap().state, SessionState::Exited);
        assert!(store.list_live_sessions().unwrap().is_empty());
        let summaries = store.task_usage_summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].usage.total(), 300, "per-task totals count every row, whatever the alias");
    }

    #[test]
    fn commands_and_hooks_are_drained_once() {
        let store = Store::open_in_memory().unwrap();
        let t = linear_task("ENG-3");
        store.insert_task(&t).unwrap();
        store.enqueue_command(&DaemonCommand::Pause { task_id: t.id }).unwrap();
        store.enqueue_command(&DaemonCommand::SyncNow).unwrap();
        let cmds = store.drain_commands().unwrap();
        assert_eq!(cmds, vec![DaemonCommand::Pause { task_id: t.id }, DaemonCommand::SyncNow]);
        assert!(store.drain_commands().unwrap().is_empty());

        store.insert_hook_event(t.id, None, HookEvent::Stop, &serde_json::json!({"last_assistant_message": "hi"})).unwrap();
        let ev = store.drain_hook_events().unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].event, HookEvent::Stop);
        assert!(store.drain_hook_events().unwrap().is_empty());
        assert_eq!(store.hook_events_for_task(t.id, 10).unwrap().len(), 1);
    }

    #[test]
    fn events_kv_and_heartbeat() {
        let store = Store::open_in_memory().unwrap();
        let t = linear_task("ENG-4");
        store.insert_task(&t).unwrap();
        store.log_event(Some(t.id), None, EventLevel::Info, "task.created", "created", serde_json::json!({})).unwrap();
        store.log_event(Some(t.id), None, EventLevel::Warn, "session.crashed", "boom", serde_json::json!({"code": 1})).unwrap();
        let ev = store.events_for_task(t.id, 10).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].kind, "task.created");
        assert_eq!(store.count_events_of_kind("session.", Utc::now() - Duration::minutes(1)).unwrap(), 1);
        assert_eq!(store.events_after(ev[0].id, 10).unwrap().len(), 1);

        store.kv_set("x", &42u32).unwrap();
        assert_eq!(store.kv_get::<u32>("x").unwrap(), Some(42));
        assert!(!store.daemon_alive(Duration::seconds(30)).unwrap());
        store.heartbeat(999).unwrap();
        assert!(store.daemon_alive(Duration::seconds(30)).unwrap());
        assert_eq!(store.daemon_heartbeat().unwrap().unwrap().0, 999);
        assert_eq!(store.integrity_check().unwrap(), "ok");
    }

    #[test]
    fn counts_by_state() {
        let store = Store::open_in_memory().unwrap();
        let mut a = linear_task("A-1");
        a.state = TaskState::Running;
        let mut b = linear_task("A-2");
        b.state = TaskState::Completed;
        store.insert_task(&a).unwrap();
        store.insert_task(&b).unwrap();
        store.insert_task(&linear_task("A-3")).unwrap();
        let c = store.counts().unwrap();
        assert_eq!((c.queued, c.running, c.completed), (1, 1, 1));
        assert_eq!(store.list_open_tasks().unwrap().len(), 2);
    }

    #[test]
    fn reset_empties_tables_and_keeps_kv_unless_everything() {
        let store = Store::open_in_memory().unwrap();
        let t = linear_task("R-1");
        store.insert_task(&t).unwrap();
        store.log_event(Some(t.id), None, EventLevel::Info, "x", "y", serde_json::json!({})).unwrap();
        store.enqueue_command(&DaemonCommand::SyncNow).unwrap();
        store.kv_set("budget.calibration.claude", &serde_json::json!({ "observed_fraction": 0.5 })).unwrap();
        let before = store.reset_counts().unwrap();
        assert_eq!((before.tasks, before.events, before.commands, before.kv), (1, 1, 1, 1));

        let removed = store.reset(false).unwrap();
        assert_eq!((removed.tasks, removed.events, removed.commands, removed.kv), (1, 1, 1, 0));
        assert!(store.list_tasks().unwrap().is_empty());
        assert!(store.recent_events(10).unwrap().is_empty());
        assert!(store.drain_commands().unwrap().is_empty());
        assert!(store.kv_get::<serde_json::Value>("budget.calibration.claude").unwrap().is_some(), "kv survives");
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

        let removed = store.reset(true).unwrap();
        assert_eq!(removed.kv, 1);
        assert!(store.kv_get::<serde_json::Value>("budget.calibration.claude").unwrap().is_none());
        assert_eq!(store.reset_counts().unwrap().total(), 0);
        assert_eq!(store.integrity_check().unwrap(), "ok");
    }

    /// The v1 schema as `powerqueue` 0.1.0 created it (no `agent_session_id`,
    /// calibration under `budget.calibration`).
    const V1_SCHEMA: &str = "
        CREATE TABLE tasks (id TEXT PRIMARY KEY, key TEXT NOT NULL UNIQUE, title TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
            source TEXT NOT NULL, source_kind TEXT NOT NULL, linear_issue_id TEXT, state TEXT NOT NULL, criticality TEXT NOT NULL,
            score REAL NOT NULL DEFAULT 0, labels TEXT NOT NULL DEFAULT '[]', linear_priority INTEGER, estimate REAL, project TEXT,
            model_override TEXT, model TEXT, worktree_path TEXT, branch TEXT, attempts INTEGER NOT NULL DEFAULT 0, max_attempts INTEGER,
            not_before TEXT, last_error TEXT, summary TEXT, score_reasons TEXT NOT NULL DEFAULT '[]', created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL, started_at TEXT, completed_at TEXT);
        CREATE TABLE sessions (id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, attempt INTEGER NOT NULL,
            model TEXT NOT NULL, state TEXT NOT NULL, tmux_session TEXT NOT NULL, tmux_window TEXT NOT NULL, pane_id TEXT, pid INTEGER,
            transcript_path TEXT, exit_code INTEGER, started_at TEXT NOT NULL, ended_at TEXT, last_activity_at TEXT NOT NULL, error TEXT);
        CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT NOT NULL);
        INSERT INTO kv (key, value, updated_at) VALUES ('budget.calibration', '{\"observed_fraction\":0.4,\"at\":\"2026-09-29T00:00:00Z\",\"measured_fraction\":0.1}', '2026-09-29T00:00:00Z');
        INSERT INTO tasks (id, key, title, source, source_kind, state, criticality, created_at, updated_at)
            VALUES ('0199a000-0000-7000-8000-000000000001', 'ENG-1', 't', '{\"kind\":\"manual\"}', 'manual', 'running', 'normal', '2026-09-29T00:00:00Z', '2026-09-29T00:00:00Z');
        INSERT INTO sessions (id, task_id, attempt, model, state, tmux_session, tmux_window, started_at, last_activity_at)
            VALUES ('6d1f0a4e-9d2d-4b57-9d2d-5c1c9b2b6c01', '0199a000-0000-7000-8000-000000000001', 1, 'opus', 'running', 'pq', '@1', '2026-09-29T00:00:00Z', '2026-09-29T00:00:00Z');
        PRAGMA user_version = 1;
    ";

    #[test]
    fn migrates_a_v1_database_file_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pq.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V1_SCHEMA).unwrap();
        }
        for pass in 0..2 {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
            let session =
                store.get_session(uuid::Uuid::parse_str("6d1f0a4e-9d2d-4b57-9d2d-5c1c9b2b6c01").unwrap()).unwrap().unwrap();
            assert_eq!(session.model, ModelTier::opus());
            let expected = if pass == 0 { None } else { Some("thread-1".to_string()) };
            assert_eq!(session.agent_session_id, expected, "pass {pass}: the column survives reopening");
            let cal: serde_json::Value = store.kv_get("budget.calibration.claude").unwrap().expect("renamed calibration");
            assert_eq!(cal["observed_fraction"].as_f64(), Some(0.4));
            assert!(store.kv_get::<serde_json::Value>("budget.calibration").unwrap().is_none());
            store.set_agent_session_id(session.id, "thread-1").unwrap();
        }
        // A legacy row that reappears next to the new key is dropped, never overwrites it.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "INSERT INTO kv VALUES ('budget.calibration', '{\"observed_fraction\":0.9}', 'x'); PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let cal: serde_json::Value = store.kv_get("budget.calibration.claude").unwrap().unwrap();
        assert_eq!(cal["observed_fraction"].as_f64(), Some(0.4));
        assert!(store.kv_get::<serde_json::Value>("budget.calibration").unwrap().is_none());
        assert_eq!(store.integrity_check().unwrap(), "ok");
    }
}
