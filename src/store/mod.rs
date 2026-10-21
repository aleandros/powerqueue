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
/// * v3: `tasks.cycle` and `tasks.cycle_number` (the Linear cycle an issue is in).
/// * v4: `tasks.blocked_by`, `tasks.children` and `tasks.parent` (Linear
///   dependencies; see [`crate::domain::LinkedIssue`]).
/// * v5: `tasks.pr_url` and `tasks.review` (the PR watcher; see
///   [`crate::domain::ReviewWatch`]).
/// * v6: `sessions.waiting_since` and `sessions.waited_secs` (time spent
///   waiting on a human does not count towards `max_session_secs`).
/// * v7: tasks.execution_project and execution_repo (stable execution binding).
/// * v8: persisted GitHub execution admission (legacy rows start unverified).
/// * v9: tasks.routing (persisted deterministic routing evidence).
/// * v10: pinned execution queue and workflow policy.
/// * v11: explicit local deferral, independent of automatic not_before.
/// * v12: explicit and automatic repository assignment provenance.
pub const SCHEMA_VERSION: i64 = 12;

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
    pub blocked: usize,
    pub needs_attention: usize,
    pub in_review: usize,
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
        let columns_of = |table: &str| -> Result<Vec<String>> {
            Ok(conn
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<String>>>()?)
        };
        if version < 2 {
            let has_column = columns_of("sessions")?.iter().any(|c| c == "agent_session_id");
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
        if version < 3 {
            let cols = columns_of("tasks")?;
            let mut added = Vec::new();
            for (name, sql) in [
                ("cycle", "ALTER TABLE tasks ADD COLUMN cycle TEXT"),
                ("cycle_number", "ALTER TABLE tasks ADD COLUMN cycle_number INTEGER"),
            ] {
                if !cols.iter().any(|c| c == name) {
                    conn.execute(sql, []).with_context(|| format!("add tasks.{name}"))?;
                    added.push(name);
                }
            }
            if !added.is_empty() {
                tracing::info!(from = version, to = SCHEMA_VERSION, columns_added = ?added, "migrated database schema to v3");
            }
        }
        if version < 4 {
            let cols = columns_of("tasks")?;
            let mut added = Vec::new();
            for (name, sql) in [
                ("blocked_by", "ALTER TABLE tasks ADD COLUMN blocked_by TEXT NOT NULL DEFAULT '[]'"),
                ("children", "ALTER TABLE tasks ADD COLUMN children TEXT NOT NULL DEFAULT '[]'"),
                ("parent", "ALTER TABLE tasks ADD COLUMN parent TEXT"),
            ] {
                if !cols.iter().any(|c| c == name) {
                    conn.execute(sql, []).with_context(|| format!("add tasks.{name}"))?;
                    added.push(name);
                }
            }
            if !added.is_empty() {
                tracing::info!(from = version, to = SCHEMA_VERSION, columns_added = ?added, "migrated database schema to v4");
            }
        }
        if version < 5 {
            let cols = columns_of("tasks")?;
            let mut added = Vec::new();
            for (name, sql) in
                [("pr_url", "ALTER TABLE tasks ADD COLUMN pr_url TEXT"), ("review", "ALTER TABLE tasks ADD COLUMN review TEXT")]
            {
                if !cols.iter().any(|c| c == name) {
                    conn.execute(sql, []).with_context(|| format!("add tasks.{name}"))?;
                    added.push(name);
                }
            }
            if !added.is_empty() {
                tracing::info!(from = version, to = SCHEMA_VERSION, columns_added = ?added, "migrated database schema to v5");
            }
        }
        if version < 6 {
            let cols = columns_of("sessions")?;
            let mut added = Vec::new();
            for (name, sql) in [
                ("waiting_since", "ALTER TABLE sessions ADD COLUMN waiting_since TEXT"),
                ("waited_secs", "ALTER TABLE sessions ADD COLUMN waited_secs INTEGER NOT NULL DEFAULT 0"),
            ] {
                if !cols.iter().any(|c| c == name) {
                    conn.execute(sql, []).with_context(|| format!("add sessions.{name}"))?;
                    added.push(name);
                }
            }
            if !added.is_empty() {
                tracing::info!(from = version, to = SCHEMA_VERSION, columns_added = ?added, "migrated database schema to v6");
            }
        }
        if version < SCHEMA_VERSION {
            let cols = columns_of("tasks")?;
            for (name, sql) in [
                ("execution_project", "ALTER TABLE tasks ADD COLUMN execution_project TEXT NOT NULL DEFAULT 'default'"),
                ("execution_repo", "ALTER TABLE tasks ADD COLUMN execution_repo TEXT"),
                ("github_admission", "ALTER TABLE tasks ADD COLUMN github_admission TEXT"),
                ("routing", "ALTER TABLE tasks ADD COLUMN routing TEXT"),
                ("project_assignment", "ALTER TABLE tasks ADD COLUMN project_assignment TEXT"),
                ("execution", "ALTER TABLE tasks ADD COLUMN execution TEXT"),
                ("deferral", "ALTER TABLE tasks ADD COLUMN deferral TEXT"),
            ] {
                if !cols.iter().any(|c| c == name) {
                    conn.execute(sql, []).with_context(|| format!("add tasks.{name}"))?;
                }
            }
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
                not_before, last_error, summary, score_reasons, created_at, updated_at, started_at, completed_at, cycle, cycle_number,
                blocked_by, children, parent, pr_url, review, execution_project, execution_repo, github_admission, routing, execution, deferral, project_assignment)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30,
                ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42)",
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
                task.cycle,
                task.cycle_number,
                json(&task.blocked_by),
                json(&task.children),
                task.parent,
                task.pr_url,
                task.review.as_ref().map(json),
                task.execution_project,
                task.execution_repo,
                task.github_admission.as_ref().map(json),
                task.routing.as_ref().map(json),
                task.execution.as_ref().map(json),
                task.deferral.as_ref().map(json),
                task.project_assignment.as_ref().map(json),
            ],
        )
        .with_context(|| format!("insert task {}", task.key))?;
        Ok(())
    }

    /// Update lifecycle/source columns; explicit deferral is only written by
    /// `set_task_deferral`, so stale daemon snapshots cannot erase a user hold.
    /// Source description edits that change deferral require `update_source_task`.
    /// `updated_at` is set to now.
    pub fn update_task(&self, task: &Task) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Source holds, like local holds, are explicit intent. Only source sync
        // may change them; a lifecycle snapshot taken before sync cannot undo one.
        let current = tx
            .query_row(
                &format!("SELECT {} FROM tasks WHERE id=?1", Self::TASK_COLS),
                params![task.id.to_string()],
                Self::row_to_task,
            )
            .context("load task before lifecycle update")?;
        let description =
            if current.source_deferral().map_err(|e| e.to_string()) != task.source_deferral().map_err(|e| e.to_string()) {
                &current.description
            } else {
                &task.description
            };
        // Assignment changes have their own transactional writers. Preserve newer
        // human/model choices when a lifecycle writer holds an older snapshot.
        let assignment = if current.project_assignment != task.project_assignment { &current } else { task };
        let n = tx.execute(
            "UPDATE tasks SET key=?2, title=?3, description=?4, source=?5, source_kind=?6, linear_issue_id=?7, state=?8,
                criticality=?9, score=?10, labels=?11, linear_priority=?12, estimate=?13, project=?14, model_override=?15,
                model=?16, worktree_path=?17, branch=?18, attempts=?19, max_attempts=?20, not_before=?21, last_error=?22,
                summary=?23, score_reasons=?24, updated_at=?25, started_at=?26, completed_at=?27, cycle=?28, cycle_number=?29,
                blocked_by=?30, children=?31, parent=?32, pr_url=?33, review=?34, execution_project=?35, execution_repo=?36, github_admission=?37, routing=?38, execution=?39, project_assignment=?40
             WHERE id=?1",
            params![
                task.id.to_string(),
                task.key,
                task.title,
                description,
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
                task.cycle,
                task.cycle_number,
                json(&task.blocked_by),
                json(&task.children),
                task.parent,
                task.pr_url,
                task.review.as_ref().map(json),
                assignment.execution_project,
                task.execution_repo,
                task.github_admission.as_ref().map(json),
                assignment.routing.as_ref().map(json),
                task.execution.as_ref().map(json),
                current.project_assignment.as_ref().map(json),
            ],
        )?;
        if n == 0 {
            return Err(anyhow!("task {} not found", task.id));
        }
        tx.commit().context("persist task lifecycle without overwriting explicit holds")?;
        Ok(())
    }

    /// Atomically apply a pure deferral transition and its log events to the latest
    /// row. Only the local hold changes; concurrent source/lifecycle writes survive.
    /// Fails if the task is absent, launching, or persistence fails (all rolled back).
    pub fn set_task_deferral(&self, id: TaskId, deferral: Option<crate::deferral::Deferral>) -> Result<Task> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut task = tx
            .query_row(&format!("SELECT {} FROM tasks WHERE id=?1", Self::TASK_COLS), params![id.to_string()], Self::row_to_task)
            .context("load task for deferral")?;
        let effects = crate::scheduler::commands::on_defer(&mut task, deferral)?;
        if !effects.is_empty() {
            task.updated_at = Utc::now();
            tx.execute(
                "UPDATE tasks SET deferral=?2, updated_at=?3 WHERE id=?1",
                params![id.to_string(), task.deferral.as_ref().map(json), ts(&task.updated_at)],
            )?;
            for effect in effects {
                if let crate::scheduler::transitions::Effect::Log { level, kind, message, data } = effect {
                    tx.execute(
                        "INSERT INTO events (task_id, timestamp, level, kind, message, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![id.to_string(), ts(&task.updated_at), level.as_str(), kind, message, json(&data)],
                    )?;
                }
            }
        }
        tx.commit().context("persist task deferral and audit event")?;
        Ok(task)
    }

    /// Atomically change an unstarted task's execution project. Refuses if a
    /// daemon started it or another command changed its assignment meanwhile.
    /// The explicit assignment and routing cache change; task state and source data are preserved.
    pub fn set_execution_project(&self, id: TaskId, from: &str, to: &str) -> Result<()> {
        let changed = self
            .lock()
            .execute(
                "UPDATE tasks SET execution_project=?3, updated_at=?4, project_assignment=?5, routing=NULL WHERE id=?1 AND execution_project=?2
             AND execution_repo IS NULL AND attempts=0 AND started_at IS NULL
             AND branch IS NULL AND worktree_path IS NULL AND review IS NULL AND pr_url IS NULL
             AND state IN ('queued','paused','blocked','throttled','needs_attention')
             AND NOT EXISTS (SELECT 1 FROM sessions WHERE task_id=?1)",
                params![id.to_string(), from, to, ts(&Utc::now()), json(&crate::routing::ProjectAssignment::Explicit { project: to.into() })],
            )
            .context("change execution project of unstarted task")?;
        if changed == 0 {
            return Err(anyhow!("task {id} changed or has execution history; project was not changed"));
        }
        Ok(())
    }

    /// Apply saved analysis against the latest task inside a write transaction.
    /// Refuses stale results, human overrides, existing sessions and execution history.
    /// Only repository assignment/evidence and their audit events are changed.
    pub fn apply_analysis_project(&self, id: TaskId, cfg: &crate::config::Config) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let applied = Self::apply_analysis_project_in(&tx, id, cfg)?;
        tx.commit().context("commit automatic repository selection")?;
        Ok(applied)
    }

    fn apply_analysis_project_in(tx: &rusqlite::Transaction<'_>, id: TaskId, cfg: &crate::config::Config) -> Result<bool> {
        let Some(mut task) = tx
            .query_row(&format!("SELECT {} FROM tasks WHERE id=?1", Self::TASK_COLS), params![id.to_string()], Self::row_to_task)
            .optional()?
        else {
            return Ok(false);
        };
        let record: Option<String> = tx
            .query_row("SELECT value FROM kv WHERE key=?1", params![crate::analysis::record_key(id)], |row| row.get(0))
            .optional()?;
        let Some(record) = record else { return Ok(false) };
        let record = serde_json::from_str(&record).context("decode saved repository analysis")?;
        let has_session =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE task_id=?1)", params![id.to_string()], |row| row.get(0))?;
        let effects = crate::scheduler::commands::on_analysis_project(&mut task, cfg, &record, has_session);
        if effects.is_empty() {
            return Ok(false);
        }
        let now = ts(&Utc::now());
        tx.execute(
            "UPDATE tasks SET execution_project=?2, project_assignment=?3, routing=?4, updated_at=?5 WHERE id=?1",
            params![
                id.to_string(),
                task.execution_project,
                task.project_assignment.as_ref().map(json),
                task.routing.as_ref().map(json),
                now
            ],
        )?;
        for effect in effects {
            if let crate::scheduler::transitions::Effect::Log { level, kind, message, data } = effect {
                tx.execute(
                    "INSERT INTO events (task_id,timestamp,level,kind,message,data) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![id.to_string(), now, level.as_str(), kind, message, json(&data)],
                )?;
            }
        }
        Ok(true)
    }

    /// Save routing evidence only while its metadata and assignment remain current.
    /// Returns false on a concurrent sync, reassignment or launch; never changes execution state.
    pub fn record_routing(&self, before: &Task, task: &Task) -> Result<bool> {
        let changed = self
            .lock()
            .execute(
                "UPDATE tasks SET routing=?2 WHERE id=?1 AND execution_project=?3 AND state=?4
             AND attempts=?5 AND routing IS ?6 AND labels=?7 AND project IS ?8 AND source=?9 AND project_assignment IS ?10",
                params![
                    task.id.to_string(),
                    task.routing.as_ref().map(json),
                    before.execution_project,
                    before.state.as_str(),
                    before.attempts,
                    before.routing.as_ref().map(json),
                    json(&before.labels),
                    before.project,
                    json(&before.source),
                    before.project_assignment.as_ref().map(json)
                ],
            )
            .context("record deterministic routing decision")?;
        Ok(changed != 0)
    }

    /// Apply source metadata and cancellation only if the inspected task is still current.
    /// A concurrent launch/reassignment wins; sync retries on the next poll.
    pub fn update_source_task(&self, before: &Task, task: &Task) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx
            .execute(
                "UPDATE tasks SET title=?2, description=?3, labels=?4, source=?5,
             linear_priority=?6, estimate=?7, project=?8, cycle=?9, cycle_number=?10,
             blocked_by=?11, children=?12, parent=?13, github_admission=?14, routing=?15,
             state=?16, last_error=?17, completed_at=?18, updated_at=?19
             WHERE id=?1 AND execution_project=?20 AND state=?21 AND attempts=?22
             AND routing IS ?23 AND github_admission IS ?24 AND updated_at=?25
             AND execution_repo IS ?26",
                params![
                    task.id.to_string(),
                    task.title,
                    task.description,
                    json(&task.labels),
                    json(&task.source),
                    task.linear_priority,
                    task.estimate,
                    task.project,
                    task.cycle,
                    task.cycle_number,
                    json(&task.blocked_by),
                    json(&task.children),
                    task.parent,
                    task.github_admission.as_ref().map(json),
                    task.routing.as_ref().map(json),
                    task.state.as_str(),
                    task.last_error,
                    task.completed_at.as_ref().map(ts),
                    ts(&Utc::now()),
                    before.execution_project,
                    before.state.as_str(),
                    before.attempts,
                    before.routing.as_ref().map(json),
                    before.github_admission.as_ref().map(json),
                    ts(&before.updated_at),
                    before.execution_repo
                ],
            )
            .context("apply source sync without overwriting concurrent execution")?;
        if changed != 0 {
            for effect in crate::scheduler::commands::source_deferral_events(before, task) {
                if let crate::scheduler::transitions::Effect::Log { level, kind, message, data } = effect {
                    tx.execute(
                        "INSERT INTO events (task_id, timestamp, level, kind, message, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![task.id.to_string(), ts(&Utc::now()), level.as_str(), kind, message, json(&data)],
                    )?;
                }
            }
        }
        tx.commit().context("persist source metadata and deferral audit")?;
        Ok(changed != 0)
    }

    /// Persist admission only if the verified task snapshot is still current.
    /// Leaves execution state untouched; returns false when a concurrent update won.
    pub fn record_github_admission(&self, before: &Task, task: &Task) -> Result<bool> {
        let changed = self
            .lock()
            .execute(
                "UPDATE tasks SET github_admission=?2, routing=?9 WHERE id=?1 AND title=?3 AND description=?4
             AND state=?5 AND attempts=?6 AND github_admission IS ?7 AND execution_project=?8
             AND routing IS ?10 AND labels=?11 AND project IS ?12 AND source=?13 AND project_assignment IS ?14",
                params![
                    task.id.to_string(),
                    task.github_admission.as_ref().map(json),
                    before.title,
                    before.description,
                    before.state.as_str(),
                    before.attempts,
                    before.github_admission.as_ref().map(json),
                    before.execution_project,
                    task.routing.as_ref().map(json),
                    before.routing.as_ref().map(json),
                    json(&before.labels),
                    before.project,
                    json(&before.source),
                    before.project_assignment.as_ref().map(json)
                ],
            )
            .context("record verified GitHub admission")?;
        Ok(changed != 0)
    }

    /// Persist the pure start transition and its claimed attempt/artifacts only
    /// if the candidate still belongs to the same project and is in the state
    /// the planner read. Returns false
    /// when another command superseded it; no repository operation may follow.
    pub fn claim_task_start(&self, before: &Task, starting: &Task) -> Result<bool> {
        self.claim_task_start_with_target(before, starting, None)
    }

    /// Claim a start and atomically remember where its agent will run. The
    /// saved target survives configuration changes and daemon restarts.
    /// Returns false for a superseded candidate; no launch may follow it.
    pub fn claim_task_start_with_target(
        &self,
        before: &Task,
        starting: &Task,
        target: Option<&crate::tmux::LaunchTarget>,
    ) -> Result<bool> {
        self.claim_task_start_with_target_at(before, starting, target, Utc::now())
    }

    /// Claim against an explicit clock instant, preserving the same transactional
    /// guards as production. Returns false for held or superseded candidates;
    /// persistence errors roll back both the claim and saved launch target.
    pub(crate) fn claim_task_start_with_target_at(
        &self,
        before: &Task,
        starting: &Task,
        target: Option<&crate::tmux::LaunchTarget>,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        if !before.execution_approved() || !starting.execution_approved() || !before.deferral_allows(now) {
            return Ok(false);
        }
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx
            .execute(
                "UPDATE tasks SET state=?5, model=?6, not_before=?7, review=?8, updated_at=?9,
             attempts=?13, branch=?14, worktree_path=?15, execution=?20
             WHERE id=?1 AND execution_project=?2 AND state=?3 AND attempts=?4
             AND github_admission IS ?10 AND title=?11 AND description=?12
             AND routing IS ?16 AND labels=?17 AND project IS ?18 AND source=?19 AND deferral IS ?21 AND project_assignment IS ?22",
                params![
                    before.id.to_string(),
                    before.execution_project,
                    before.state.as_str(),
                    before.attempts,
                    starting.state.as_str(),
                    starting.model.as_ref().map(|m| m.alias()),
                    starting.not_before.as_ref().map(ts),
                    starting.review.as_ref().map(json),
                    ts(&now),
                    before.github_admission.as_ref().map(json),
                    before.title,
                    before.description,
                    starting.attempts,
                    starting.branch,
                    starting.worktree_path,
                    before.routing.as_ref().map(json),
                    json(&before.labels),
                    before.project,
                    json(&before.source),
                    starting.execution.as_ref().map(json),
                    before.deferral.as_ref().map(json),
                    before.project_assignment.as_ref().map(json)
                ],
            )
            .context("claim task start in its execution project")?;
        if changed != 0
            && let Some(target) = target
        {
            tx.execute(
                "INSERT INTO kv(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
                params![crate::tmux::LaunchTarget::key(before.id), json(target), ts(&now)],
            )
            .context("remember launch tmux target")?;
        }
        tx.commit().context("commit task start and tmux target")?;
        Ok(changed != 0)
    }

    /// Commit a pure abandoned-start recovery only while its entire observed
    /// task row is unchanged and no live session has appeared. An immediate
    /// transaction serializes the check with CLI/source writers and session
    /// insertion, including other connections.
    /// Returns false for superseded observations; errors leave the row unchanged.
    pub fn recover_task_start(&self, before: &Task, recovered: &Task) -> Result<bool> {
        if before.state != TaskState::Starting || recovered.id != before.id {
            return Ok(false);
        }
        let mut conn = self.lock();
        let tx =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).context("begin abandoned start recovery")?;
        let sql = format!("SELECT {} FROM tasks WHERE id=?1", Self::TASK_COLS);
        let current = tx.query_row(&sql, [before.id.to_string()], Self::row_to_task).optional()?;
        if current.as_ref() != Some(before) {
            return Ok(false);
        }
        let occupied: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE task_id=?1
             AND state IN ('launching','running','idle'))",
            params![before.id.to_string()],
            |r| r.get(0),
        )?;
        if occupied {
            return Ok(false);
        }
        tx.execute(
            "UPDATE tasks SET state=?2, not_before=?3, last_error=?4, completed_at=?5, updated_at=?6 WHERE id=?1",
            params![
                before.id.to_string(),
                recovered.state.as_str(),
                recovered.not_before.as_ref().map(ts),
                recovered.last_error,
                recovered.completed_at.as_ref().map(ts),
                ts(&Utc::now())
            ],
        )
        .context("persist abandoned start recovery")?;
        tx.commit().context("commit abandoned start recovery")?;
        Ok(true)
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
        let blocked_by: String = row.get("blocked_by")?;
        let children: String = row.get("children")?;
        let review: Option<String> = row.get("review")?;
        Ok(Task {
            id: TaskId::from_str(&id).map_err(|e| conv(e.into()))?,
            key: row.get("key")?,
            title: row.get("title")?,
            description: row.get("description")?,
            source: from_json(&source).map_err(conv)?,
            github_admission: row
                .get::<_, Option<String>>("github_admission")?
                .as_deref()
                .map(from_json)
                .transpose()
                .map_err(conv)?,
            routing: row.get::<_, Option<String>>("routing")?.as_deref().map(from_json).transpose().map_err(conv)?,
            execution_project: row.get("execution_project")?,
            project_assignment: row
                .get::<_, Option<String>>("project_assignment")?
                .as_deref()
                .map(from_json)
                .transpose()
                .map_err(conv)?,
            execution_repo: row.get("execution_repo")?,
            execution: row.get::<_, Option<String>>("execution")?.as_deref().map(from_json).transpose().map_err(conv)?,
            state: TaskState::from_str(&state).map_err(|e| conv(anyhow!(e)))?,
            criticality: Criticality::from_str(&criticality).map_err(|e| conv(anyhow!(e)))?,
            score: row.get("score")?,
            labels: from_json(&labels).map_err(conv)?,
            linear_priority: row.get("linear_priority")?,
            estimate: row.get("estimate")?,
            project: row.get("project")?,
            cycle: row.get("cycle")?,
            cycle_number: row.get("cycle_number")?,
            blocked_by: from_json(&blocked_by).map_err(conv)?,
            children: from_json(&children).map_err(conv)?,
            parent: row.get("parent")?,
            model_override: model_override.map(|m| ModelTier::from_str(&m)).transpose().map_err(|e| conv(anyhow!(e)))?,
            model: model.map(|m| ModelTier::from_str(&m)).transpose().map_err(|e| conv(anyhow!(e)))?,
            worktree_path: row.get("worktree_path")?,
            branch: row.get("branch")?,
            attempts: row.get("attempts")?,
            max_attempts: row.get("max_attempts")?,
            not_before: opt_ts(row.get("not_before")?).map_err(conv)?,
            deferral: row.get::<_, Option<String>>("deferral")?.as_deref().map(from_json).transpose().map_err(conv)?,
            last_error: row.get("last_error")?,
            summary: row.get("summary")?,
            score_reasons: from_json(&reasons).map_err(conv)?,
            created_at: parse_ts(&created_at).map_err(conv)?,
            updated_at: parse_ts(&updated_at).map_err(conv)?,
            started_at: opt_ts(row.get("started_at")?).map_err(conv)?,
            completed_at: opt_ts(row.get("completed_at")?).map_err(conv)?,
            pr_url: row.get("pr_url")?,
            review: review.as_deref().map(from_json).transpose().map_err(conv)?,
        })
    }

    const TASK_COLS: &'static str =
        "id, key, title, description, source, source_kind, linear_issue_id, state, criticality, score, labels,
        linear_priority, estimate, project, model_override, model, worktree_path, branch, attempts, max_attempts, not_before,
        last_error, summary, score_reasons, created_at, updated_at, started_at, completed_at, cycle, cycle_number,
        blocked_by, children, parent, pr_url, review, execution_project, execution_repo, github_admission, routing, execution, deferral, project_assignment";

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
                TaskState::Blocked => c.blocked += 1,
                TaskState::NeedsAttention => c.needs_attention += 1,
                TaskState::InReview => c.in_review += 1,
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
                exit_code, started_at, ended_at, last_activity_at, error, agent_session_id, waiting_since, waited_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
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
                s.waiting_since.as_ref().map(ts),
                s.waited_secs,
            ],
        )?;
        Ok(())
    }

    pub fn update_session(&self, s: &Session) -> Result<()> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE sessions SET task_id=?2, attempt=?3, model=?4, state=?5, tmux_session=?6, tmux_window=?7, pane_id=?8, pid=?9,
                transcript_path=?10, exit_code=?11, started_at=?12, ended_at=?13, last_activity_at=?14, error=?15,
                agent_session_id=?16, waiting_since=?17, waited_secs=?18 WHERE id=?1",
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
                s.waiting_since.as_ref().map(ts),
                s.waited_secs,
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
            waiting_since: opt_ts(row.get("waiting_since")?).map_err(conv)?,
            waited_secs: row.get("waited_secs")?,
        })
    }

    const SESSION_COLS: &'static str =
        "id, task_id, attempt, model, state, tmux_session, tmux_window, pane_id, pid, transcript_path,
        exit_code, started_at, ended_at, last_activity_at, error, agent_session_id, waiting_since, waited_secs";

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

    /// The most recently started session for a task, if any. Attempts reset
    /// on a terminal-task retry, so they cannot order execution history. For
    /// equal start timestamps, prefer the row inserted later. Fails on a DB error.
    pub fn latest_session(&self, task_id: TaskId) -> Result<Option<Session>> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {} FROM sessions WHERE task_id = ?1 ORDER BY started_at DESC, rowid DESC LIMIT 1",
            Self::SESSION_COLS
        );
        Ok(conn.query_row(&sql, params![task_id.to_string()], Self::row_to_session).optional()?)
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
        let sql = format!("SELECT {} FROM sessions ORDER BY started_at ASC, rowid ASC", Self::SESSION_COLS);
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

    /// Usage per tier per minute (bucket start, oldest first) between two
    /// instants; the budget ledger turns this into a cumulative series so it
    /// can sum spend between any two readings without one query per pair.
    /// Rows with an unknown model alias are skipped like in `usage_by_tier`.
    pub fn usage_by_minute(&self, since: DateTime<Utc>, until: DateTime<Utc>) -> Result<Vec<(DateTime<Utc>, TierUsage)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT substr(timestamp, 1, 16) AS minute, tier, SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_creation_input_tokens), SUM(cache_read_input_tokens), COUNT(*)
             FROM usage WHERE timestamp >= ?1 AND timestamp < ?2 GROUP BY minute, tier ORDER BY minute",
        )?;
        let rows = stmt.query_map(params![ts(&since), ts(&until)], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                TokenUsage {
                    input_tokens: r.get::<_, i64>(2)? as u64,
                    output_tokens: r.get::<_, i64>(3)? as u64,
                    cache_creation_input_tokens: r.get::<_, i64>(4)? as u64,
                    cache_read_input_tokens: r.get::<_, i64>(5)? as u64,
                },
                r.get::<_, i64>(6)? as u64,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (minute, tier, usage, messages) = row?;
            let at = parse_ts(&format!("{minute}:00Z"))?;
            match ModelTier::from_str(&tier) {
                Ok(tier) => out.push((at, TierUsage { tier, usage, messages })),
                Err(e) => tracing::warn!(tier = %tier, messages, error = %e, "skipping usage rows with an unknown model alias"),
            }
        }
        Ok(out)
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

    /// The task's most recent event of exactly `kind`, if any.
    pub fn last_task_event(&self, task_id: TaskId, kind: &str) -> Result<Option<Event>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, task_id, session_id, timestamp, level, kind, message, data FROM events
             WHERE task_id = ?1 AND kind = ?2 ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![task_id.to_string(), kind], Self::row_to_event)?;
        Ok(rows.next().transpose()?)
    }

    /// The most recent event of exactly `kind` across all tasks, if any.
    pub fn last_event_of_kind(&self, kind: &str) -> Result<Option<Event>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, task_id, session_id, timestamp, level, kind, message, data FROM events
             WHERE kind = ?1 ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![kind], Self::row_to_event)?;
        Ok(rows.next().transpose()?)
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

    /// Save advisory analysis and its audit event atomically without updating task rows.
    /// Returns false if a newer analysis already finished; fails for missing tasks or storage errors.
    pub fn save_analysis(&self, id: TaskId, record: &crate::analysis::AnalysisRecord) -> Result<bool> {
        self.save_analysis_inner(id, record, None).map(|(saved, _)| saved)
    }

    /// Save a result and reconcile automatic assignment atomically against the latest task.
    /// Returns (saved, assignment_changed); older results are ignored. Storage errors roll back both.
    pub fn save_and_apply_analysis(
        &self,
        id: TaskId,
        record: &crate::analysis::AnalysisRecord,
        cfg: &crate::config::Config,
    ) -> Result<(bool, bool)> {
        self.save_analysis_inner(id, record, Some(cfg))
    }

    fn save_analysis_inner(
        &self,
        id: TaskId,
        record: &crate::analysis::AnalysisRecord,
        cfg: Option<&crate::config::Config>,
    ) -> Result<(bool, bool)> {
        let mut conn = self.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .context("begin analysis result transaction")?;
        let exists: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)", params![id.to_string()], |r| r.get(0))?;
        if !exists {
            anyhow::bail!("cannot save analysis for a missing task");
        }
        let key = crate::analysis::record_key(id);
        let previous: Option<String> =
            tx.query_row("SELECT value FROM kv WHERE key = ?1", params![key], |r| r.get(0)).optional()?;
        if let Some(previous) = previous {
            let previous: crate::analysis::AnalysisRecord = serde_json::from_str(&previous).context("read prior analysis")?;
            if previous.created_at > record.created_at {
                return Ok((false, false));
            }
        }
        let now = ts(&Utc::now());
        tx.execute("INSERT INTO kv (key, value, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, serde_json::to_string(record)?, now])?;
        tx.execute("INSERT INTO events (task_id, timestamp, level, kind, message, data) VALUES (?1, ?2, 'info', 'analysis.recorded', ?3, ?4)",
            params![id.to_string(), now, "Advisory analysis recorded; human approval required", serde_json::to_string(record)?])?;
        let applied = if let Some(cfg) = cfg { Self::apply_analysis_project_in(&tx, id, cfg)? } else { false };
        tx.commit().context("commit analysis result and event")?;
        Ok((true, applied))
    }

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

    /// The daemon-wide scheduling pause (`powerqueue pause`), if one is
    /// set. Fails only when the database cannot be read.
    pub fn scheduling_pause(&self) -> Result<Option<SchedulingPause>> {
        self.kv_get(SCHEDULING_PAUSE_KEY)
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
            waiting_since: None,
            waited_secs: 0,
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

    #[test]
    fn migrates_v9_execution_binding_without_reclassifying_existing_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pq.sqlite");
        let task = linear_task("ENG-legacy");
        {
            let store = Store::open(&path).unwrap();
            store.insert_task(&task).unwrap();
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("ALTER TABLE tasks DROP COLUMN execution; PRAGMA user_version = 9;").unwrap();
        }
        for _ in 0..2 {
            let store = Store::open(&path).unwrap();
            let restored = store.get_task(task.id).unwrap().unwrap();
            assert!(restored.execution.is_none());
            assert_eq!(restored.state, task.state);
            assert_eq!(restored.execution_project, task.execution_project);
            assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        }
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
        // v1 → v3 in one go also added the cycle columns.
        let t = store.get_task_by_key("ENG-1").unwrap().unwrap();
        assert_eq!((t.cycle, t.cycle_number), (None, None));
    }

    /// The v2 schema (0.2.0): `agent_session_id` present, no cycle columns.
    const V2_SCHEMA: &str = "
        CREATE TABLE tasks (id TEXT PRIMARY KEY, key TEXT NOT NULL UNIQUE, title TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
            source TEXT NOT NULL, source_kind TEXT NOT NULL, linear_issue_id TEXT, state TEXT NOT NULL, criticality TEXT NOT NULL,
            score REAL NOT NULL DEFAULT 0, labels TEXT NOT NULL DEFAULT '[]', linear_priority INTEGER, estimate REAL, project TEXT,
            model_override TEXT, model TEXT, worktree_path TEXT, branch TEXT, attempts INTEGER NOT NULL DEFAULT 0, max_attempts INTEGER,
            not_before TEXT, last_error TEXT, summary TEXT, score_reasons TEXT NOT NULL DEFAULT '[]', created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL, started_at TEXT, completed_at TEXT);
        CREATE TABLE sessions (id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, attempt INTEGER NOT NULL,
            model TEXT NOT NULL, state TEXT NOT NULL, tmux_session TEXT NOT NULL, tmux_window TEXT NOT NULL, pane_id TEXT, pid INTEGER,
            transcript_path TEXT, exit_code INTEGER, started_at TEXT NOT NULL, ended_at TEXT, last_activity_at TEXT NOT NULL, error TEXT,
            agent_session_id TEXT);
        CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT NOT NULL);
        INSERT INTO kv (key, value, updated_at) VALUES ('budget.calibration.claude', '{\"observed_fraction\":0.4,\"at\":\"2026-09-29T00:00:00Z\",\"measured_fraction\":0.1}', '2026-09-29T00:00:00Z');
        INSERT INTO tasks (id, key, title, source, source_kind, state, criticality, created_at, updated_at)
            VALUES ('0199a000-0000-7000-8000-000000000002', 'ENG-2', 't', '{\"kind\":\"manual\"}', 'manual', 'queued', 'normal', '2026-09-29T00:00:00Z', '2026-09-29T00:00:00Z');
        PRAGMA user_version = 2;
    ";

    #[test]
    fn migrates_a_v2_database_file_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pq.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
        }
        for pass in 0..2 {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
            let mut t = store.get_task_by_key("ENG-2").unwrap().unwrap();
            let expected = if pass == 0 { (None, None) } else { (Some("active".to_string()), Some(12)) };
            assert_eq!((t.cycle.clone(), t.cycle_number), expected, "pass {pass}: the columns survive reopening");
            t.cycle = Some("active".into());
            t.cycle_number = Some(12);
            store.update_task(&t).unwrap();
            // The calibration key was already in its v2 place and is left alone.
            let cal: serde_json::Value = store.kv_get("budget.calibration.claude").unwrap().unwrap();
            assert_eq!(cal["observed_fraction"].as_f64(), Some(0.4));
        }
        // A database whose user_version was reset but which already has the columns is not broken by the ALTERs.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 2;").unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        let t = store.get_task_by_key("ENG-2").unwrap().unwrap();
        assert_eq!(t.cycle_number, Some(12));
        assert_eq!(store.integrity_check().unwrap(), "ok");
    }

    #[test]
    fn cycle_fields_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let mut t = linear_task("A-1");
        t.cycle = Some("next".into());
        t.cycle_number = Some(3);
        store.insert_task(&t).unwrap();
        let back = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(back.cycle.as_deref(), Some("next"));
        assert_eq!(back.cycle_number, Some(3));
        t.cycle = None;
        t.cycle_number = None;
        store.update_task(&t).unwrap();
        let back = store.get_task(t.id).unwrap().unwrap();
        assert_eq!((back.cycle, back.cycle_number), (None, None));
    }

    #[test]
    fn dependency_fields_round_trip_and_migrate_from_v3() {
        let store = Store::open_in_memory().unwrap();
        let mut t = linear_task("A-2");
        t.blocked_by =
            vec![LinkedIssue { key: "A-1".into(), title: "first".into(), state_type: "started".into(), pr_merged: true }];
        t.children =
            vec![LinkedIssue { key: "A-3".into(), title: String::new(), state_type: "completed".into(), pr_merged: false }];
        t.parent = Some("A-0".into());
        t.state = TaskState::Blocked;
        store.insert_task(&t).unwrap();
        let back = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(
            (back.blocked_by.clone(), back.children.clone(), back.parent.clone()),
            (t.blocked_by.clone(), t.children.clone(), t.parent.clone())
        );
        assert_eq!(back.state, TaskState::Blocked);
        assert_eq!(store.counts().unwrap().blocked, 1);
        t.blocked_by.clear();
        t.parent = None;
        store.update_task(&t).unwrap();
        let back = store.get_task(t.id).unwrap().unwrap();
        assert!(back.blocked_by.is_empty() && back.parent.is_none());

        // A v3 file (cycle columns, no dependency columns) gains them with empty defaults.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pq.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.execute_batch("ALTER TABLE tasks ADD COLUMN cycle TEXT; ALTER TABLE tasks ADD COLUMN cycle_number INTEGER; PRAGMA user_version = 3;")
                .unwrap();
        }
        for _ in 0..2 {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
            let t = store.get_task_by_key("ENG-2").unwrap().unwrap();
            assert!(t.blocked_by.is_empty() && t.children.is_empty() && t.parent.is_none());
        }
    }

    #[test]
    fn review_fields_round_trip_and_migrate_from_v4() {
        let store = Store::open_in_memory().unwrap();
        let mut t = linear_task("A-5");
        t.state = TaskState::InReview;
        t.pr_url = Some("https://github.com/o/r/pull/7".into());
        let mut watch = ReviewWatch::armed(Utc::now(), None);
        watch.rounds = 2;
        watch.relaunch =
            Some(ReviewRelaunch { pr_number: 7, reason: "conflict".into(), detail: String::new(), requested_at: Utc::now() });
        t.review = Some(watch.clone());
        store.insert_task(&t).unwrap();
        let back = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(back.state, TaskState::InReview);
        assert_eq!(back.pr_url, t.pr_url);
        assert_eq!(back.review.as_ref().map(|r| r.rounds), Some(2));
        assert_eq!(back.review_relaunch().map(|r| r.reason.as_str()), Some("conflict"));
        assert_eq!(store.counts().unwrap().in_review, 1);
        t.review = None;
        t.pr_url = None;
        store.update_task(&t).unwrap();
        let back = store.get_task(t.id).unwrap().unwrap();
        assert!(back.review.is_none() && back.pr_url.is_none());

        // A v4 file gains the review columns, empty.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pq.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.execute_batch(
                "ALTER TABLE tasks ADD COLUMN cycle TEXT; ALTER TABLE tasks ADD COLUMN cycle_number INTEGER;
                 ALTER TABLE tasks ADD COLUMN blocked_by TEXT NOT NULL DEFAULT '[]';
                 ALTER TABLE tasks ADD COLUMN children TEXT NOT NULL DEFAULT '[]';
                 ALTER TABLE tasks ADD COLUMN parent TEXT; PRAGMA user_version = 4;",
            )
            .unwrap();
        }
        for _ in 0..2 {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
            let t = store.get_task_by_key("ENG-2").unwrap().unwrap();
            assert!(t.pr_url.is_none() && t.review.is_none());
        }
    }
}
