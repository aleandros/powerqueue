//! `powerqueue status` — one-shot queue table.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use comfy_table::Cell;
use owo_colors::{OwoColorize, Stream};
use serde::Serialize;

use crate::cli::output::{self, human_bytes, human_duration, human_f64};
use crate::cli::{Context, StatusArgs};
use crate::domain::{Provider, ResourceSample, Session, Task, TaskState, TokenUsage};
use crate::store::{QueueCounts, Store};

/// Heartbeats older than this mean the daemon is not running.
pub const DAEMON_ALIVE_SECS: i64 = 30;

/// Fail with a friendly hint when `config.toml` does not exist yet.
pub fn ensure_initialised(ctx: &Context) -> Result<()> {
    if ctx.is_initialised() {
        Ok(())
    } else {
        Err(anyhow!(
            "powerqueue is not set up yet ({} is missing). Run `powerqueue init` first.",
            ctx.paths.config_file().display()
        ))
    }
}

/// One task with the extra columns `status` shows.
#[derive(Debug, Clone, Serialize)]
pub struct StatusRow {
    #[serde(flatten)]
    pub task: Task,
    /// Provider of the task's model (chosen or forced), if any.
    pub provider: Option<Provider>,
    pub session: Option<Session>,
    pub usage: TokenUsage,
    pub weighted_tokens: f64,
    pub resource: Option<ResourceSample>,
}

/// Daemon liveness as derived from the heartbeat row.
#[derive(Debug, Clone, Serialize)]
pub struct DaemonStatus {
    pub alive: bool,
    pub pid: Option<u32>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub heartbeat_age_secs: Option<i64>,
}

/// Read the heartbeat and decide whether the daemon is alive (`now - at < DAEMON_ALIVE_SECS`).
pub fn daemon_status(store: &Store, now: DateTime<Utc>) -> Result<DaemonStatus> {
    Ok(match store.daemon_heartbeat()? {
        Some((pid, at)) => {
            let age = (now - at).num_seconds();
            DaemonStatus {
                alive: now - at < Duration::seconds(DAEMON_ALIVE_SECS),
                pid: Some(pid),
                heartbeat_at: Some(at),
                heartbeat_age_secs: Some(age),
            }
        }
        None => DaemonStatus { alive: false, pid: None, heartbeat_at: None, heartbeat_age_secs: None },
    })
}

/// Sort tasks for display: live sessions first, then by score (desc), then by age.
pub fn sort_for_display(tasks: &mut [Task]) {
    tasks.sort_by(|a, b| {
        b.state
            .has_live_session()
            .cmp(&a.state.has_live_session())
            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.created_at.cmp(&b.created_at))
    });
}

/// Load the rows `status` shows. Terminal tasks are hidden unless `all`.
pub fn load_rows(store: &Store, all: bool) -> Result<Vec<StatusRow>> {
    let mut tasks = if all { store.list_tasks()? } else { store.list_open_tasks()? };
    sort_for_display(&mut tasks);
    let mut rows = Vec::with_capacity(tasks.len());
    for task in tasks {
        let session = store.latest_session(task.id)?;
        let usage = store.usage_for_task(task.id)?;
        let resource = match &session {
            Some(s) if s.state.is_live() => store.latest_resource_sample(s.id)?,
            _ => None,
        };
        let provider = super::task::effective_model(&task).map(|m| m.provider());
        rows.push(StatusRow { weighted_tokens: usage.weighted(), provider, task, session, usage, resource });
    }
    Ok(rows)
}

/// Age (queued) or runtime (live) column.
pub fn age_or_runtime(task: &Task, now: DateTime<Utc>) -> String {
    if task.state.has_live_session()
        && let Some(started) = task.started_at
    {
        return human_duration((now - started).num_seconds());
    }
    human_duration((now - task.created_at).num_seconds())
}

/// `12% / 1.3 GB` for a live session with a sample, `-` otherwise.
pub fn cpu_rss(sample: Option<&ResourceSample>) -> String {
    match sample {
        Some(s) => format!("{:.0}% / {}", s.cpu_percent, human_bytes(s.rss_bytes)),
        None => "-".to_string(),
    }
}

/// The "waiting on" column: pending blocker keys, or the open sub-issues
/// of a container (`parent (done)` once they are all done, `parent (all
/// canceled)` when none was completed); for a task in review its PR
/// (`PR #7`, `PR #7 (waiting for manual merge)`), for a pending review round
/// the reason (`PR #7: conflict`); `-` otherwise.
pub fn waiting_on(task: &Task) -> String {
    let keys = task.waiting_on();
    let pr = || {
        let url = task.pr_url.as_deref().unwrap_or_default();
        url.parse::<crate::github::PrRef>().map(|p| format!("PR #{}", p.number)).unwrap_or_else(|_| url.to_string())
    };
    if task.state == TaskState::InReview {
        if task.review.as_ref().is_some_and(|r| r.waiting_manual_merge) {
            format!("{} (waiting for manual merge)", pr())
        } else {
            pr()
        }
    } else if let Some(r) = task.review_relaunch().filter(|_| matches!(task.state, TaskState::Queued | TaskState::Starting)) {
        format!("{}: {}", pr(), r.reason)
    } else if !keys.is_empty() {
        keys.join(", ")
    } else if task.container_all_canceled() {
        "parent (all canceled)".to_string()
    } else if task.is_container() {
        "parent (done)".to_string()
    } else {
        "-".to_string()
    }
}

fn counts_line(c: &QueueCounts) -> String {
    let mut parts = vec![format!("queued {}", c.queued), format!("running {}", c.running)];
    for (label, n) in [
        ("in review", c.in_review),
        ("idle", c.idle),
        ("attention", c.needs_attention),
        ("crashed", c.crashed),
        ("throttled", c.throttled),
        ("paused", c.paused),
        ("blocked", c.blocked),
        ("done", c.completed),
        ("failed", c.failed),
        ("cancelled", c.cancelled),
    ] {
        if n > 0 {
            parts.push(format!("{label} {n}"));
        }
    }
    parts.join(" · ")
}

/// Width to lay the table out in: the terminal's when stdout is a TTY,
/// otherwise a generous fixed width so piped output is not truncated.
fn terminal_width() -> usize {
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        crossterm::terminal::size().map(|(w, _)| w as usize).ok().filter(|w| *w >= 40).unwrap_or(120)
    } else {
        160
    }
}

/// Handle `powerqueue status`.
pub fn run(ctx: &mut Context, args: StatusArgs) -> Result<i32> {
    ensure_initialised(ctx)?;
    let cfg = ctx.config_cloned()?;
    let store = ctx.store()?.clone();
    let now = Utc::now();
    let rows = load_rows(&store, args.all)?;
    let daemon = daemon_status(&store, now)?;
    let counts = store.counts()?;
    let pause = store.scheduling_pause()?;

    if ctx.json {
        let out = serde_json::json!({
            "daemon": daemon,
            "scheduling_paused": pause,
            "repo_overrides_error": cfg.overrides.error,
            "tmux_session": cfg.tmux.session_name,
            "counts": {
                "queued": counts.queued, "running": counts.running, "idle": counts.idle,
                "crashed": counts.crashed, "throttled": counts.throttled, "paused": counts.paused, "blocked": counts.blocked,
                "needs_attention": counts.needs_attention, "in_review": counts.in_review, "completed": counts.completed,
                "failed": counts.failed, "cancelled": counts.cancelled,
            },
            "tasks": rows,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }

    let color = ctx.color;
    let daemon_text = match (daemon.alive, daemon.pid, daemon.heartbeat_age_secs) {
        (true, pid, age) => {
            let s = format!("● running (pid {}, heartbeat {} ago)", pid.unwrap_or(0), human_duration(age.unwrap_or(0)));
            if color { s.if_supports_color(Stream::Stdout, |t| t.green()).to_string() } else { s }
        }
        (false, _, Some(age)) => {
            let s = format!("○ not running (last heartbeat {} ago)", human_duration(age));
            if color { s.if_supports_color(Stream::Stdout, |t| t.red()).to_string() } else { s }
        }
        (false, _, None) => {
            let s = "○ never started".to_string();
            if color { s.if_supports_color(Stream::Stdout, |t| t.red()).to_string() } else { s }
        }
    };
    let pause_text = match &pause {
        Some(p) => {
            let s = format!("  ·  scheduling {} (`powerqueue resume`)", p.describe());
            if color { s.if_supports_color(Stream::Stdout, |t| t.yellow()).to_string() } else { s }
        }
        None => String::new(),
    };
    println!("daemon {daemon_text}  ·  {}  ·  tmux session {}{pause_text}", counts_line(&counts), cfg.tmux.session_name);
    if let Some(error) = &cfg.overrides.error {
        let s = format!("warning: {error}; shown with config.toml alone (`powerqueue doctor` explains)");
        println!("{}", if color { s.if_supports_color(Stream::Stdout, |t| t.yellow()).to_string() } else { s });
    }

    if rows.is_empty() {
        println!(
            "{}",
            if args.all { "no tasks yet" } else { "no open tasks (use --all to include finished ones)" }
                .if_supports_color(Stream::Stdout, |t| t.dimmed())
        );
        return Ok(0);
    }

    let width = terminal_width();
    let waiting = rows
        .iter()
        .any(|r| r.task.is_waiting() || !r.task.blocked_by.is_empty() || r.task.pr_url.is_some() && !r.task.state.is_terminal());
    let title_max = width.saturating_sub(if waiting { 92 } else { 78 }).clamp(16, 80);
    let mut table = output::table();
    table.set_width(width as u16);
    if !color {
        table.force_no_tty();
    }
    let mut header = vec!["KEY", "STATE", "CRIT", "MODEL", "ATTEMPT", "WEIGHTED TOKENS", "CPU/RSS", "AGE/RUNTIME"];
    if waiting {
        header.push("WAITING ON");
    }
    header.push("TITLE");
    table.set_header(header);
    for row in &rows {
        let t = &row.task;
        let state = if color { output::state_colored(t.state) } else { t.state.to_string() };
        let crit = if color { output::criticality_colored(t.criticality) } else { t.criticality.to_string() };
        let model = super::task::effective_model(t);
        let model_text = if color {
            output::model_colored_with_provider(model)
        } else {
            model.map(output::model_with_provider).unwrap_or("-".into())
        };
        let model_text = if t.model_override.is_some() { format!("{model_text}*") } else { model_text };
        let attempts = match t.max_attempts {
            Some(max) => format!("{}/{}", t.attempts, max),
            None => t.attempts.to_string(),
        };
        let tokens = if row.usage.is_zero() { "-".to_string() } else { human_f64(row.weighted_tokens) };
        let mut cells = vec![
            Cell::new(&t.key),
            Cell::new(state),
            Cell::new(crit),
            Cell::new(model_text),
            Cell::new(attempts),
            Cell::new(tokens),
            Cell::new(cpu_rss(row.resource.as_ref())),
            Cell::new(age_or_runtime(t, now)),
        ];
        if waiting {
            cells.push(Cell::new(waiting_on(t)));
        }
        cells.push(Cell::new(output::truncate(&t.title, title_max)));
        table.add_row(cells);
    }
    println!("{table}");
    if rows.iter().any(|r| r.task.model_override.is_some()) {
        println!("{}", "* model forced by the user or PRIORITY.md".if_supports_color(Stream::Stdout, |t| t.dimmed()));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{TaskSource, TaskState};

    fn task(key: &str, state: TaskState, score: f64) -> Task {
        let mut t = Task::new(key, key, TaskSource::Manual);
        t.state = state;
        t.score = score;
        t
    }

    #[test]
    fn live_tasks_sort_first_then_score() {
        let mut tasks = vec![
            task("a", TaskState::Queued, 900.0),
            task("b", TaskState::Running, 10.0),
            task("c", TaskState::Queued, 1000.0),
            task("d", TaskState::Idle, 5.0),
        ];
        sort_for_display(&mut tasks);
        let keys: Vec<_> = tasks.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, vec!["b", "d", "c", "a"]);
    }

    #[test]
    fn rows_hide_terminal_tasks_unless_all() {
        let store = Store::open_in_memory().unwrap();
        store.insert_task(&task("open", TaskState::Queued, 1.0)).unwrap();
        store.insert_task(&task("done", TaskState::Completed, 1.0)).unwrap();
        assert_eq!(load_rows(&store, false).unwrap().len(), 1);
        assert_eq!(load_rows(&store, true).unwrap().len(), 2);
    }

    #[test]
    fn daemon_status_from_heartbeat() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        assert!(!daemon_status(&store, now).unwrap().alive);
        store.heartbeat(42).unwrap();
        let st = daemon_status(&store, now).unwrap();
        assert!(st.alive);
        assert_eq!(st.pid, Some(42));
        let later = now + Duration::seconds(DAEMON_ALIVE_SECS + 5);
        assert!(!daemon_status(&store, later).unwrap().alive);
    }

    #[test]
    fn waiting_on_column() {
        let linked = |key: &str, state_type: &str| crate::domain::LinkedIssue {
            key: key.into(),
            title: String::new(),
            state_type: state_type.into(),
            pr_merged: false,
        };
        let mut t = task("a", TaskState::Blocked, 1.0);
        assert_eq!(waiting_on(&t), "-");
        t.blocked_by = vec![linked("B-1", "started"), linked("B-2", "unstarted"), linked("B-3", "completed")];
        assert_eq!(waiting_on(&t), "B-1, B-2");
        t.blocked_by.clear();
        t.children = vec![linked("C-1", "completed"), linked("C-2", "started")];
        assert_eq!(waiting_on(&t), "C-2");
        t.children[1].state_type = "completed".into();
        assert_eq!(waiting_on(&t), "parent (done)");
        for c in &mut t.children {
            c.state_type = "canceled".into();
        }
        assert_eq!(waiting_on(&t), "parent (all canceled)");

        let mut r = task("r", TaskState::InReview, 1.0);
        r.pr_url = Some("https://github.com/o/r/pull/7".into());
        r.review = Some(crate::domain::ReviewWatch::armed(Utc::now(), None));
        assert_eq!(waiting_on(&r), "PR #7");
        r.review.as_mut().unwrap().waiting_manual_merge = true;
        assert_eq!(waiting_on(&r), "PR #7 (waiting for manual merge)");
        r.state = TaskState::Queued;
        r.review.as_mut().unwrap().relaunch = Some(crate::domain::ReviewRelaunch {
            pr_number: 7,
            reason: "conflict".into(),
            detail: String::new(),
            requested_at: Utc::now(),
        });
        assert_eq!(waiting_on(&r), "PR #7: conflict");
    }

    #[test]
    fn age_and_cpu_columns() {
        let now = Utc::now();
        let mut t = task("x", TaskState::Running, 1.0);
        t.started_at = Some(now - Duration::seconds(125));
        t.created_at = now - Duration::hours(3);
        assert_eq!(age_or_runtime(&t, now), "2m05s");
        t.state = TaskState::Queued;
        assert_eq!(age_or_runtime(&t, now), "3h00m");
        assert_eq!(cpu_rss(None), "-");
        let s = ResourceSample {
            session_id: uuid::Uuid::new_v4(),
            task_id: t.id,
            timestamp: now,
            cpu_percent: 12.4,
            rss_bytes: 1536,
            process_count: 2,
        };
        assert_eq!(cpu_rss(Some(&s)), "12% / 1.5 KB");
    }
}
