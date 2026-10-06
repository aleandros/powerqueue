//! `powerqueue task ...` — inspect and control individual tasks.
//!
//! Control commands go through the daemon's command queue. When no daemon is
//! running (no heartbeat within 30 s) the state change is also applied
//! directly so the CLI stays useful offline. `complete` and `block` always
//! write directly: Claude runs them from inside the session and the daemon
//! picks the new state up on its next tick.

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::Utc;
use comfy_table::Cell;
use owo_colors::{OwoColorize, Stream, Style};

use crate::cli::output::{self, human_bytes, human_f64, model_with_provider};
use crate::cli::{Context, TaskCommand, TaskRef};
use crate::config::Config;
use crate::domain::{DaemonCommand, Event, EventLevel, ModelTier, ReviewWatch, Session, Task, TaskState};
use crate::github::PrRef;
use crate::store::Store;
use crate::tmux::Tmux;

use super::status::{daemon_status, ensure_initialised};

/// Resolve a task reference (key, id or id prefix) or fail with a hint.
pub fn find_task(store: &Store, needle: &str) -> Result<Task> {
    store.find_task(needle)?.ok_or_else(|| anyhow!("no task matches `{needle}` (try `powerqueue status --all`)"))
}

/// Mark a task completed directly in the store (no daemon required).
pub fn complete_task(store: &Store, task: &mut Task, summary: Option<&str>) -> Result<()> {
    if task.state == TaskState::Completed {
        bail!("{} is already completed", task.key);
    }
    if !task.state.can_transition_to(TaskState::Completed) {
        bail!("cannot complete {} while it is {} (cancel it instead)", task.key, task.state);
    }
    task.state = TaskState::Completed;
    task.completed_at = Some(Utc::now());
    if let Some(s) = summary.map(str::trim).filter(|s| !s.is_empty()) {
        task.summary = Some(s.to_string());
    }
    task.last_error = None;
    store.update_task(task)?;
    store.log_event(
        Some(task.id),
        None,
        EventLevel::Info,
        "task.completed_by_command",
        "marked completed via `powerqueue task complete`",
        serde_json::json!({ "summary": task.summary }),
    )?;
    Ok(())
}

/// Hand a task off for review (`task complete --pr <url>`): it moves to
/// `in_review` with the PR recorded, so the daemon releases its session,
/// slot and worktree (keeping the branch) and watches the PR. Calling it
/// again while in review re-arms the watch (e.g. after a review round).
/// Fails for a URL that is not a GitHub pull request or a task that is not
/// running (queued, finished, ...).
pub fn hand_off_for_review(store: &Store, task: &mut Task, summary: Option<&str>, pr_url: &str) -> Result<()> {
    let pr: PrRef = pr_url.parse().map_err(|e: String| anyhow!(e))?;
    if task.state != TaskState::InReview && !task.state.can_transition_to(TaskState::InReview) {
        bail!("cannot hand {} off for review while it is {} (only a running task opens a PR)", task.key, task.state);
    }
    let now = Utc::now();
    let mut watch = ReviewWatch::armed(now, task.review.as_ref());
    watch.worktree_path = task.worktree_path.clone().or_else(|| task.review.as_ref().and_then(|r| r.worktree_path.clone()));
    let from = task.state;
    task.state = TaskState::InReview;
    task.pr_url = Some(pr_url.trim().to_string());
    task.review = Some(watch);
    task.not_before = None;
    task.last_error = None;
    if let Some(s) = summary.map(str::trim).filter(|s| !s.is_empty()) {
        task.summary = Some(s.to_string());
    }
    store.update_task(task)?;
    let rounds = task.review.as_ref().map_or(0, |r| r.rounds);
    store.log_event(
        Some(task.id),
        None,
        EventLevel::Info,
        "task.in_review",
        &format!(
            "handed off for review: {pr} (merge armed{})",
            if rounds > 0 { format!(", after {rounds} review round(s)") } else { String::new() }
        ),
        serde_json::json!({ "pr": task.pr_url, "from": from, "rounds": rounds, "summary": task.summary }),
    )?;
    Ok(())
}

/// Mark a task as needing a human directly in the store.
pub fn block_task(store: &Store, task: &mut Task, reason: Option<&str>) -> Result<()> {
    if task.state.is_terminal() {
        bail!("cannot block {}: it is already {}", task.key, task.state);
    }
    if !task.state.can_transition_to(TaskState::NeedsAttention) {
        bail!("cannot block {} while it is {}", task.key, task.state);
    }
    task.state = TaskState::NeedsAttention;
    if let Some(r) = reason.map(str::trim).filter(|r| !r.is_empty()) {
        task.last_error = Some(r.to_string());
    }
    store.update_task(task)?;
    store.log_event(
        Some(task.id),
        None,
        EventLevel::Warn,
        "task.blocked_by_command",
        &format!("blocked via `powerqueue task block`: {}", task.last_error.as_deref().unwrap_or("no reason given")),
        serde_json::json!({ "reason": task.last_error }),
    )?;
    Ok(())
}

/// The state a control command moves a task to when applied offline.
pub fn offline_target(cmd: &DaemonCommand) -> Option<TaskState> {
    match cmd {
        DaemonCommand::Cancel { .. } => Some(TaskState::Cancelled),
        DaemonCommand::Pause { .. } => Some(TaskState::Paused),
        DaemonCommand::Resume { .. } | DaemonCommand::Retry { .. } => Some(TaskState::Queued),
        _ => None,
    }
}

/// Apply a control command directly (used when no daemon is running).
/// Returns `Ok(false)` when the transition is not allowed from the current state.
pub fn apply_offline(store: &Store, task: &mut Task, cmd: &DaemonCommand) -> Result<bool> {
    let Some(mut target) = offline_target(cmd) else { return Ok(false) };
    if matches!(cmd, DaemonCommand::Resume { .. }) && task.parked_in_review() {
        target = TaskState::InReview;
        if let Some(watch) = task.review.as_mut() {
            watch.rearm(Utc::now());
        }
        task.last_error = None;
    }
    if task.state == target {
        return Ok(true);
    }
    if !task.state.can_transition_to(target) {
        return Ok(false);
    }
    let from = task.state;
    task.state = target;
    match cmd {
        DaemonCommand::Retry { .. } => {
            task.not_before = None;
            task.last_error = None;
            task.completed_at = None;
        }
        DaemonCommand::Cancel { .. } => task.completed_at = Some(Utc::now()),
        _ => {}
    }
    store.update_task(task)?;
    let kind = match cmd {
        DaemonCommand::Cancel { .. } => "task.cancelled",
        DaemonCommand::Pause { .. } => "task.paused",
        DaemonCommand::Resume { .. } => "task.resumed",
        _ => "task.retried",
    };
    store.log_event(
        Some(task.id),
        None,
        EventLevel::Info,
        kind,
        &format!("{from} → {target} (applied by the CLI, no daemon running)"),
        serde_json::json!({ "from": from, "to": target, "offline": true }),
    )?;
    Ok(true)
}

fn control(ctx: &mut Context, task_ref: &TaskRef, make: impl Fn(Task) -> DaemonCommand, verb: &str) -> Result<i32> {
    let store = ctx.store()?.clone();
    let mut task = find_task(&store, &task_ref.task)?;
    let cmd = make(task.clone());
    store.enqueue_command(&cmd)?;
    let daemon = daemon_status(&store, Utc::now())?;
    if daemon.alive {
        println!(
            "{} {} queued for the daemon ({} is {})",
            verb.if_supports_color(Stream::Stdout, |t| t.green()),
            task.key.if_supports_color(Stream::Stdout, |t| t.bold()),
            task.key,
            task.state
        );
    } else if apply_offline(&store, &mut task, &cmd)? {
        println!(
            "{} {} — no daemon running, applied directly ({} is now {})",
            verb.if_supports_color(Stream::Stdout, |t| t.green()),
            task.key.if_supports_color(Stream::Stdout, |t| t.bold()),
            task.key,
            task.state
        );
    } else {
        println!(
            "{} {} queued — no daemon running and `{}` cannot be applied offline from state {}; it will run when the daemon starts",
            verb.if_supports_color(Stream::Stdout, |t| t.yellow()),
            task.key.if_supports_color(Stream::Stdout, |t| t.bold()),
            verb,
            task.state
        );
    }
    Ok(0)
}

fn tmux_for(ctx: &mut Context) -> Result<Tmux> {
    let cfg = ctx.config_cloned()?;
    Ok(Tmux::new(cfg.tmux.binary.clone(), cfg.tmux.socket_name.clone()))
}

fn live_pane(store: &Store, task: &Task) -> Result<(Session, String)> {
    let session =
        store.latest_session(task.id)?.ok_or_else(|| anyhow!("{} has no session yet (state {})", task.key, task.state))?;
    let pane =
        session.pane_id.clone().ok_or_else(|| anyhow!("session {} of {} has no tmux pane recorded", session.id, task.key))?;
    Ok((session, pane))
}

fn paint(color: bool, s: String, plain: &str) -> String {
    if color { s } else { plain.to_string() }
}

fn level_colored(level: EventLevel, color: bool) -> String {
    let s = level.as_str();
    if !color {
        return s.to_string();
    }
    match level {
        EventLevel::Debug => s.if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string(),
        EventLevel::Info => s.if_supports_color(Stream::Stdout, |t| t.cyan()).to_string(),
        EventLevel::Warn => s.if_supports_color(Stream::Stdout, |t| t.yellow()).to_string(),
        EventLevel::Error => s.if_supports_color(Stream::Stdout, |t| t.style(Style::new().red().bold())).to_string(),
    }
}

fn opt_ts(ts: Option<chrono::DateTime<Utc>>) -> String {
    ts.map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_else(|| "-".to_string())
}

fn show(ctx: &mut Context, task_ref: &TaskRef) -> Result<i32> {
    let store = ctx.store()?.clone();
    let task = find_task(&store, &task_ref.task)?;
    let sessions = store.list_sessions_for_task(task.id)?;
    let history = store.events_for_task(task.id, 2000)?;
    let events = history[history.len().saturating_sub(50)..].to_vec();
    let pr_events = if task.pr_url.is_some() { pr_timeline(&history) } else { Vec::new() };
    let base = session_base(&history);
    let rounds_max = ctx.config_or_default()?.scheduler.review_rounds_max;
    let usage = store.usage_for_task(task.id)?;
    let mut session_rows = Vec::with_capacity(sessions.len());
    for s in &sessions {
        let u = store.usage_for_session(s.id)?;
        let stats = store.resource_stats(s.id)?;
        session_rows.push((s.clone(), u, stats));
    }

    if ctx.json {
        let out = serde_json::json!({
            "task": task,
            "provider": effective_model(&task).map(|m| m.provider()),
            "usage": usage,
            "weighted_tokens": usage.weighted(),
            "sessions": session_rows.iter().map(|(s, u, stats)| serde_json::json!({
                "session": s,
                "usage": u,
                "weighted_tokens": u.weighted(),
                "peak_rss_bytes": stats.map(|x| x.0),
                "avg_cpu_percent": stats.map(|x| x.1),
                "samples": stats.map(|x| x.2),
            })).collect::<Vec<_>>(),
            "events": events,
            "pr_timeline": pr_events,
            "base": base.map(|(reference, sha)| serde_json::json!({ "base": reference, "base_sha": sha })),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }

    let color = ctx.color;
    let kv = |k: &str, v: String| {
        println!(
            "  {:<14} {}",
            if color { k.if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string() } else { k.to_string() },
            v
        )
    };
    println!("{} {}", task.key.if_supports_color(Stream::Stdout, |t| t.bold()), task.title);
    kv("id", task.id.to_string());
    kv("state", paint(color, output::state_colored(task.state), task.state.as_str()));
    kv("criticality", paint(color, output::criticality_colored(task.criticality), task.criticality.as_str()));
    kv("score", format!("{:.1}", task.score));
    for r in &task.score_reasons {
        println!(
            "  {:<14} {}",
            "",
            if color {
                format!("· {r}").if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string()
            } else {
                format!("· {r}")
            }
        );
    }
    let model_line = match (&task.model, &task.model_override) {
        (m, Some(o)) => {
            format!("{} (forced: {})", m.as_ref().map(model_with_provider).unwrap_or("-".into()), model_with_provider(o))
        }
        (Some(m), None) => model_with_provider(m),
        (None, None) => "auto".to_string(),
    };
    kv("model", model_line);
    let source = match &task.source {
        crate::domain::TaskSource::Linear { identifier, url, team_key, .. } => format!("linear {identifier} ({team_key}) {url}"),
        crate::domain::TaskSource::Manual => "manual".to_string(),
    };
    kv("source", source);
    if !task.labels.is_empty() {
        kv("labels", task.labels.join(", "));
    }
    if let Some(p) = &task.project {
        kv("project", p.clone());
    }
    if let Some(c) = &task.cycle {
        kv(
            "cycle",
            match task.cycle_number {
                Some(n) => format!("{c} (#{n})"),
                None => c.clone(),
            },
        );
    }
    if let Some(e) = task.estimate {
        kv("estimate", format!("{e}"));
    }
    if let Some(p) = &task.parent {
        kv("parent", p.clone());
    }
    if !task.blocked_by.is_empty() {
        kv("blocked by", task.blocked_by.iter().map(|b| b.key.as_str()).collect::<Vec<_>>().join(", "));
    }
    if !task.children.is_empty() {
        kv("sub-issues", task.children.iter().map(|c| c.key.as_str()).collect::<Vec<_>>().join(", "));
    }
    if task.is_waiting() {
        let on = task.waiting_on();
        kv("waiting on", if on.is_empty() { "-".to_string() } else { on.join(", ") });
    }
    kv("branch", task.branch.clone().unwrap_or_else(|| "-".into()));
    kv("worktree", task.worktree_path.clone().unwrap_or_else(|| "-".into()));
    if let Some(url) = &task.pr_url {
        kv("pull request", url.clone());
        if let Some(watch) = &task.review {
            let mut status = vec![format!("review round {}/{rounds_max}", watch.rounds)];
            if watch.waiting_manual_merge {
                status.push("waiting for a manual merge".to_string());
            }
            if let Some(r) = &watch.relaunch {
                status.push(format!(
                    "relaunch pending: {}{}",
                    r.reason,
                    if r.detail.is_empty() { String::new() } else { format!(" {}", r.detail) }
                ));
            }
            if let Some(at) = watch.last_polled_at {
                status.push(format!("last checked {}", at.with_timezone(&chrono::Local).format("%m-%d %H:%M")));
            }
            kv("review", status.join("; "));
        }
    }
    let attempts = match task.max_attempts {
        Some(max) => format!("{}/{}", task.attempts, max),
        None => task.attempts.to_string(),
    };
    kv("attempts", attempts);
    kv(
        "tokens",
        if usage.is_zero() {
            "-".into()
        } else {
            format!("{} weighted ({} raw)", human_f64(usage.weighted()), human_f64(usage.total() as f64))
        },
    );
    if !usage.is_zero() {
        kv(
            "token breakdown",
            format!(
                "{} input + {} output + {} cache write + {} cache read (all calls, all attempts)",
                usage.input_tokens, usage.output_tokens, usage.cache_creation_input_tokens, usage.cache_read_input_tokens
            ),
        );
    }
    if let Some((reference, sha)) = &base {
        kv("base", format!("{reference} at {sha}"));
    }
    kv("created", opt_ts(Some(task.created_at)));
    kv("updated", opt_ts(Some(task.updated_at)));
    kv("started", opt_ts(task.started_at));
    kv("completed", opt_ts(task.completed_at));
    if let Some(nb) = task.not_before {
        kv("not before", opt_ts(Some(nb)));
    }
    if let Some(e) = &task.last_error {
        kv("last error", if color { e.if_supports_color(Stream::Stdout, |t| t.red()).to_string() } else { e.clone() });
    }
    if let Some(s) = &task.summary {
        kv("summary", s.clone());
    }

    if !session_rows.is_empty() {
        println!("\n{}", "sessions".if_supports_color(Stream::Stdout, |t| t.bold()));
        let mut table = output::table();
        if !color {
            table.force_no_tty();
        }
        table.set_header(vec![
            "ATTEMPT",
            "MODEL",
            "STATE",
            "PID",
            "STARTED",
            "ENDED",
            "EXIT",
            "WEIGHTED TOKENS",
            "PEAK RSS",
            "AVG CPU",
        ]);
        for (s, u, stats) in &session_rows {
            table.add_row(vec![
                Cell::new(s.attempt),
                Cell::new(paint(color, output::model_colored(Some(&s.model)), s.model.as_str())),
                Cell::new(s.state.to_string()),
                Cell::new(s.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())),
                Cell::new(s.started_at.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string()),
                Cell::new(
                    s.ended_at
                        .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "-".into()),
                ),
                Cell::new(s.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "-".into())),
                Cell::new(if u.is_zero() { "-".into() } else { human_f64(u.weighted()) }),
                Cell::new(stats.map(|x| human_bytes(x.0)).unwrap_or_else(|| "-".into())),
                Cell::new(stats.map(|x| format!("{:.0}%", x.1)).unwrap_or_else(|| "-".into())),
            ]);
        }
        println!("{table}");
    }

    if !pr_events.is_empty() {
        println!("\n{}", "pull request".if_supports_color(Stream::Stdout, |t| t.bold()));
        for e in &pr_events {
            println!("{}", format_event(e, color));
        }
    }

    if !events.is_empty() {
        println!("\n{}", "timeline".if_supports_color(Stream::Stdout, |t| t.bold()));
        for e in &events {
            println!("{}", format_event(e, color));
        }
    }
    Ok(0)
}

/// The commit the task's branch was created from — `(ref, sha)` from the
/// last `worktree.ready` event that created a branch — or `None` before the
/// first launch (and for tasks started by versions that did not record it).
pub fn session_base(events: &[Event]) -> Option<(String, String)> {
    events
        .iter()
        .rev()
        .filter(|e| e.kind == "worktree.ready")
        .find_map(|e| Some((e.data["base"].as_str()?.to_string(), e.data["base_sha"].as_str()?.to_string())))
}

/// The pull request's story: hand-offs, what the watcher saw and did,
/// review-round launches, the final branch deletion.
pub fn pr_timeline(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|e| {
            e.kind == "task.in_review"
                || e.kind.starts_with("review.")
                || e.kind == "session.released"
                || e.kind == "cleanup.branch_deleted"
                || (e.kind == "session.launched" && !e.data["review"].is_null())
        })
        .cloned()
        .collect()
}

/// `HH:MM:SS LEVEL kind message` with the level coloured.
pub fn format_event(e: &Event, color: bool) -> String {
    let when = e.timestamp.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S");
    let kind = if color { e.kind.if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string() } else { e.kind.clone() };
    format!("  {when} {:<5} {kind} {}", level_colored(e.level, color), e.message)
}

fn explain(ctx: &mut Context, task_ref: &TaskRef) -> Result<i32> {
    let store = ctx.store()?.clone();
    let task = find_task(&store, &task_ref.task)?;
    let events = store.events_for_task(task.id, 500)?;
    let throttled = events.iter().rev().find(|e| e.kind == "task.throttled");
    let chosen = events.iter().rev().find(|e| e.kind == "task.model_chosen" || e.kind == "task.starting");
    if ctx.json {
        let out = serde_json::json!({
            "key": task.key,
            "id": task.id,
            "state": task.state,
            "criticality": task.criticality,
            "score": task.score,
            "score_reasons": task.score_reasons,
            "model": task.model,
            "model_override": task.model_override,
            "provider": effective_model(&task).map(|m| m.provider()),
            "not_before": task.not_before,
            "waiting_on": task.waiting_on(),
            "blocked_by": task.blocked_by,
            "children": task.children,
            "parent": task.parent,
            "last_throttled": throttled,
            "last_model_decision": chosen,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    let color = ctx.color;
    println!("{} {}", task.key.if_supports_color(Stream::Stdout, |t| t.bold()), task.title);
    println!("  state        {}", paint(color, output::state_colored(task.state), task.state.as_str()));
    println!("  criticality  {}", paint(color, output::criticality_colored(task.criticality), task.criticality.as_str()));
    println!("  score        {:.1}", task.score);
    if task.score_reasons.is_empty() {
        println!(
            "    {}",
            "(not scored yet — the daemon scores tasks against PRIORITY.md on its next tick)"
                .if_supports_color(Stream::Stdout, |t| t.dimmed())
        );
    }
    for r in &task.score_reasons {
        println!("    · {r}");
    }
    if let Some(o) = &task.model_override {
        println!("  model        forced to {}", model_with_provider(o));
    } else {
        println!(
            "  model        {} (chosen by the budget policy)",
            task.model.as_ref().map(model_with_provider).unwrap_or("not chosen yet".into())
        );
    }
    if let Some(nb) = task.not_before {
        println!("  not before   {}", opt_ts(Some(nb)));
    }
    for line in dependency_lines(&task) {
        println!("{line}");
    }
    match chosen {
        Some(e) => {
            println!("\n{}", "last model decision".if_supports_color(Stream::Stdout, |t| t.bold()));
            println!("{}", format_event(e, color));
            print_data(&e.data);
        }
        None => println!("\n{}", "no model decision recorded yet".if_supports_color(Stream::Stdout, |t| t.dimmed())),
    }
    match throttled {
        Some(e) => {
            println!("\n{}", "last throttle".if_supports_color(Stream::Stdout, |t| t.bold()));
            println!("{}", format_event(e, color));
            print_data(&e.data);
        }
        None => println!("{}", "never throttled".if_supports_color(Stream::Stdout, |t| t.dimmed())),
    }
    Ok(0)
}

/// The "dependencies" block of `task explain`: why the task waits (or
/// that nothing holds it back), one line per blocker / sub-issue. Empty
/// when the task has no Linear relations at all.
pub fn dependency_lines(task: &Task) -> Vec<String> {
    if task.blocked_by.is_empty() && task.children.is_empty() && task.parent.is_none() {
        return Vec::new();
    }
    let mut out = vec![String::new(), "dependencies".to_string()];
    if task.is_container() {
        let open = task.waiting_on();
        out.push(if task.container_all_canceled() {
            format!(
                "  parent of {} sub-issue(s), all canceled: powerqueue leaves it to you (close it in Linear, or reopen or add a sub-issue)",
                task.children.len()
            )
        } else if open.is_empty() {
            format!(
                "  parent of {} sub-issue(s), all done; the daemon closes it on its next parent check (linear.done_state_parent)",
                task.children.len()
            )
        } else {
            format!(
                "  parent of {} sub-issue(s): never scheduled; waiting on {} to close it",
                task.children.len(),
                open.join(", ")
            )
        });
        for c in &task.children {
            let mark = if c.is_closed() { "done" } else { "open" };
            out.push(format!("    · {} {} ({mark}, {})", c.key, c.title, c.state_type));
        }
    }
    if !task.blocked_by.is_empty() {
        let pending = task.pending_blockers();
        out.push(if pending.is_empty() {
            "  blocked by nothing pending: every blocker is done or its PR is merged".to_string()
        } else {
            format!(
                "  waiting on {}: not scheduled until each is Done/Canceled in Linear or its PR is merged",
                pending.iter().map(|b| b.key.as_str()).collect::<Vec<_>>().join(", ")
            )
        });
        for b in &task.blocked_by {
            let why = if b.is_closed() {
                format!("satisfied, {}", b.state_type)
            } else if b.pr_merged {
                "satisfied, PR merged".to_string()
            } else {
                format!("pending, {}", if b.state_type.is_empty() { "unknown state" } else { b.state_type.as_str() })
            };
            out.push(format!("    · {} {} ({why})", b.key, b.title));
        }
    }
    if let Some(parent) = &task.parent {
        out.push(format!("  sub-issue of {parent}"));
    }
    out
}

fn print_data(data: &serde_json::Value) {
    match data {
        serde_json::Value::Object(map) if !map.is_empty() => {
            for (k, v) in map {
                match v {
                    serde_json::Value::Array(items) => {
                        println!("    {k}:");
                        for item in items {
                            println!("      · {}", item.as_str().map(str::to_string).unwrap_or_else(|| item.to_string()));
                        }
                    }
                    other => println!("    {k}: {}", other.as_str().map(str::to_string).unwrap_or_else(|| other.to_string())),
                }
            }
        }
        _ => {}
    }
}

/// The model a task runs (or will run) on: the chosen one, else the override.
pub fn effective_model(task: &Task) -> Option<&ModelTier> {
    task.model.as_ref().or(task.model_override.as_ref())
}

/// Warn (on stderr) when a forced model belongs to a provider that is not
/// enabled: the policy will never pick it, so the task would wait forever.
pub fn warn_if_provider_disabled(cfg: &Config, model: &ModelTier) {
    let p = model.provider();
    if !cfg.budget.provider(p).enabled {
        output::print_warning(&format!(
            "{p} is disabled in config (budget.providers.{p}.enabled = false); the policy will not pick {model} until you enable it"
        ));
    }
}

/// Parse the `task model` argument: `auto` clears the override.
pub fn parse_model_arg(s: &str) -> Result<Option<ModelTier>> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" | "none" | "clear" => Ok(None),
        other => other.parse::<ModelTier>().map(Some).map_err(|e| anyhow!(e)),
    }
}

/// Render the prompt the next attempt of `task` would receive: the template
/// (global or repo override) with attempt `attempts + 1`, the task's last
/// error, its forced/last model (else `budget.default_model`), and the
/// branch and worktree it would get. Nothing is written or launched.
pub fn preview_prompt(cfg: &Config, paths: &crate::paths::Paths, task: &Task) -> crate::session::RenderedPrompt {
    let model = effective_model(task).cloned().unwrap_or_else(|| cfg.budget.default_model.clone());
    let mut preview = task.clone();
    if preview.branch.is_none() {
        preview.branch = Some(crate::worktree::branch_name(&cfg.repo.branch_template, &task.slug(), &task.id.short()));
    }
    if preview.worktree_path.is_none() {
        preview.worktree_path =
            Some(crate::scheduler::worktree_dir(&cfg.worktree_root(paths), task).to_string_lossy().to_string());
    }
    let ctx = crate::session::PromptContext {
        provider: model.provider(),
        model: Some(&model),
        attempt: task.attempts.saturating_add(1),
        previous_error: task.last_error.as_deref(),
    };
    crate::session::render_prompt(&preview, cfg, &ctx)
}

fn prompt(ctx: &mut Context, task_ref: &TaskRef) -> Result<i32> {
    let cfg = ctx.config_cloned()?;
    let store = ctx.store()?.clone();
    let task = find_task(&store, &task_ref.task)?;
    let rendered = preview_prompt(&cfg, &ctx.paths, &task);
    if ctx.json {
        let out = serde_json::json!({
            "task": task.key,
            "task_id": task.id,
            "template": rendered.template,
            "prompt": rendered.text,
            "warnings": rendered.warnings,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    for w in &rendered.warnings {
        output::print_warning(w);
    }
    print!("{}", rendered.text);
    if !rendered.text.ends_with('\n') {
        println!();
    }
    Ok(0)
}

/// Handle `powerqueue task ...`.
pub fn run(ctx: &mut Context, cmd: TaskCommand) -> Result<i32> {
    ensure_initialised(ctx)?;
    match cmd {
        TaskCommand::List(args) => super::status::run(ctx, args),
        TaskCommand::Show(t) => show(ctx, &t),
        TaskCommand::Explain(t) => explain(ctx, &t),
        TaskCommand::Prompt(t) => prompt(ctx, &t),
        TaskCommand::Complete { task, summary, pr } => {
            let store = ctx.store()?.clone();
            let mut t = find_task(&store, &task.task)?;
            if let Some(url) = pr.as_deref() {
                hand_off_for_review(&store, &mut t, summary.as_deref(), url)?;
            } else {
                complete_task(&store, &mut t, summary.as_deref())?;
            }
            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&t)?);
            } else if let Some(url) = pr.as_deref() {
                println!(
                    "{} {} is in review ({url}); the daemon releases its slot and worktree and watches the PR",
                    "in review".if_supports_color(Stream::Stdout, |t| t.style(Style::new().blue().bold())),
                    t.key.if_supports_color(Stream::Stdout, |t| t.bold())
                );
            } else {
                println!(
                    "{} {} marked completed; the daemon will clean up its worktree and update Linear",
                    "done".if_supports_color(Stream::Stdout, |t| t.style(Style::new().green().bold())),
                    t.key.if_supports_color(Stream::Stdout, |t| t.bold())
                );
            }
            Ok(0)
        }
        TaskCommand::Block { task, reason } => {
            let store = ctx.store()?.clone();
            let mut t = find_task(&store, &task.task)?;
            block_task(&store, &mut t, reason.as_deref())?;
            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&t)?);
            } else {
                println!(
                    "{} {} needs attention: {}",
                    "blocked".if_supports_color(Stream::Stdout, |t| t.style(Style::new().yellow().bold())),
                    t.key.if_supports_color(Stream::Stdout, |t| t.bold()),
                    t.last_error.as_deref().unwrap_or("no reason given")
                );
            }
            Ok(0)
        }
        TaskCommand::Cancel(t) => control(ctx, &t, |task| DaemonCommand::Cancel { task_id: task.id }, "cancel"),
        TaskCommand::Pause(t) => control(ctx, &t, |task| DaemonCommand::Pause { task_id: task.id }, "pause"),
        TaskCommand::Resume(t) => control(ctx, &t, |task| DaemonCommand::Resume { task_id: task.id }, "resume"),
        TaskCommand::Retry(t) => control(ctx, &t, |task| DaemonCommand::Retry { task_id: task.id }, "retry"),
        TaskCommand::Model { task, model } => {
            let tier = parse_model_arg(&model)?;
            if let Some(m) = &tier {
                let cfg = ctx.config_cloned()?;
                warn_if_provider_disabled(&cfg, m);
            }
            let store = ctx.store()?.clone();
            let mut t = find_task(&store, &task.task)?;
            store.enqueue_command(&DaemonCommand::SetModel { task_id: t.id, model: tier.clone() })?;
            t.model_override = tier.clone();
            store.update_task(&t)?;
            store.log_event(
                Some(t.id),
                None,
                EventLevel::Info,
                "task.model_set",
                &match &tier {
                    Some(m) => format!("model forced to {m} for the next attempt"),
                    None => "model override cleared (auto)".to_string(),
                },
                serde_json::json!({ "model": tier }),
            )?;
            match tier {
                Some(m) => println!(
                    "{} {} will use {} on its next attempt",
                    "ok".if_supports_color(Stream::Stdout, |t| t.green()),
                    t.key.if_supports_color(Stream::Stdout, |t| t.bold()),
                    model_with_provider(&m)
                ),
                None => println!(
                    "{} {} will let the budget policy choose its model",
                    "ok".if_supports_color(Stream::Stdout, |t| t.green()),
                    t.key.if_supports_color(Stream::Stdout, |t| t.bold())
                ),
            }
            Ok(0)
        }
        TaskCommand::Output { task, lines } => {
            let store = ctx.store()?.clone();
            let t = find_task(&store, &task.task)?;
            let (_, pane) = live_pane(&store, &t)?;
            let tmux = tmux_for(ctx)?;
            let text = tmux.capture_pane(&pane, lines).with_context(|| format!("capture tmux pane {pane} of {}", t.key))?;
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            Ok(0)
        }
        TaskCommand::Send { task, message } => {
            let store = ctx.store()?.clone();
            let t = find_task(&store, &task.task)?;
            let (session, pane) = live_pane(&store, &t)?;
            if !session.state.is_live() {
                bail!("session {} of {} is {}; nothing is listening", session.id, t.key, session.state);
            }
            let tmux = tmux_for(ctx)?;
            tmux.send_text(&pane, &message).with_context(|| format!("send text to pane {pane} of {}", t.key))?;
            store.log_event(
                Some(t.id),
                Some(session.id),
                EventLevel::Info,
                "task.message_sent",
                "message sent to the session via `powerqueue task send`",
                serde_json::json!({ "chars": message.chars().count() }),
            )?;
            println!(
                "{} sent to {}",
                "ok".if_supports_color(Stream::Stdout, |t| t.green()),
                t.key.if_supports_color(Stream::Stdout, |t| t.bold())
            );
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskSource;

    fn stored(store: &Store, key: &str, state: TaskState) -> Task {
        let mut t = Task::new(key, key, TaskSource::Manual);
        t.state = state;
        store.insert_task(&t).unwrap();
        t
    }

    #[test]
    fn complete_sets_state_and_logs() {
        let store = Store::open_in_memory().unwrap();
        let mut t = stored(&store, "A-1", TaskState::Running);
        complete_task(&store, &mut t, Some(" all done ")).unwrap();
        let back = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(back.state, TaskState::Completed);
        assert_eq!(back.summary.as_deref(), Some("all done"));
        assert!(back.completed_at.is_some());
        let ev = store.events_for_task(t.id, 10).unwrap();
        assert_eq!(ev.last().unwrap().kind, "task.completed_by_command");
        assert!(complete_task(&store, &mut t, None).is_err());
    }

    #[test]
    fn session_base_is_the_last_created_branch_base() {
        let store = Store::open_in_memory().unwrap();
        let t = stored(&store, "B-1", TaskState::Running);
        let ready = |data: serde_json::Value| {
            store.log_event(Some(t.id), None, EventLevel::Info, "worktree.ready", "ready", data).unwrap();
        };
        assert_eq!(session_base(&store.events_for_task(t.id, 10).unwrap()), None);
        // Recorded by an older version: no base.
        ready(serde_json::json!({ "path": "/w", "branch": "pq/b-1", "base": "main" }));
        assert_eq!(session_base(&store.events_for_task(t.id, 10).unwrap()), None);
        ready(serde_json::json!({ "new_branch": true, "base": "origin/main", "base_sha": "abc" }));
        // A relaunch on the existing branch keeps the original base.
        ready(serde_json::json!({ "new_branch": false, "base": null, "base_sha": null }));
        assert_eq!(session_base(&store.events_for_task(t.id, 10).unwrap()), Some(("origin/main".to_string(), "abc".to_string())));
    }

    #[test]
    fn complete_rejects_queued_and_block_rejects_terminal() {
        let store = Store::open_in_memory().unwrap();
        let mut q = stored(&store, "Q", TaskState::Queued);
        assert!(complete_task(&store, &mut q, None).is_err());
        let mut done = stored(&store, "D", TaskState::Completed);
        assert!(block_task(&store, &mut done, Some("x")).is_err());
        let mut r = stored(&store, "R", TaskState::Running);
        block_task(&store, &mut r, Some("need creds")).unwrap();
        assert_eq!(store.get_task(r.id).unwrap().unwrap().state, TaskState::NeedsAttention);
        assert_eq!(store.events_for_task(r.id, 10).unwrap()[0].kind, "task.blocked_by_command");
    }

    #[test]
    fn complete_with_pr_hands_off_for_review_and_resume_watches_again() {
        let store = Store::open_in_memory().unwrap();
        let mut r = stored(&store, "R", TaskState::Running);
        r.worktree_path = Some("/wt/r".into());
        let url = "https://github.com/o/r/pull/7";
        assert!(hand_off_for_review(&store, &mut r, None, "https://example.com/x").is_err(), "not a PR URL");
        hand_off_for_review(&store, &mut r, Some("armed"), url).unwrap();
        let back = store.get_task(r.id).unwrap().unwrap();
        assert_eq!(back.state, TaskState::InReview);
        assert_eq!(back.pr_url.as_deref(), Some(url));
        assert_eq!(back.summary.as_deref(), Some("armed"));
        let watch = back.review.clone().unwrap();
        assert_eq!((watch.rounds, watch.worktree_path.as_deref()), (0, Some("/wt/r")));
        assert_eq!(store.events_for_task(r.id, 10).unwrap().last().unwrap().kind, "task.in_review");
        let timeline = pr_timeline(&store.events_for_task(r.id, 10).unwrap());
        assert_eq!(timeline.len(), 1);

        // Re-arming after a review round keeps the round count.
        let mut again = back.clone();
        again.state = TaskState::Running;
        again.review.as_mut().unwrap().rounds = 2;
        hand_off_for_review(&store, &mut again, None, url).unwrap();
        assert_eq!(again.review.as_ref().map(|w| (w.rounds, w.relaunch.is_none())), Some((2, true)));

        // Parked by the watcher (stale): an offline resume goes back to watching.
        again.state = TaskState::NeedsAttention;
        again.last_error = Some("stale".into());
        store.update_task(&again).unwrap();
        let resume = DaemonCommand::Resume { task_id: again.id };
        assert!(apply_offline(&store, &mut again, &resume).unwrap());
        assert_eq!(store.get_task(again.id).unwrap().unwrap().state, TaskState::InReview);

        let mut q = stored(&store, "Q", TaskState::Queued);
        assert!(hand_off_for_review(&store, &mut q, None, url).is_err(), "a queued task has no PR to hand off");
    }

    #[test]
    fn offline_apply_respects_transitions() {
        let store = Store::open_in_memory().unwrap();
        let mut t = stored(&store, "T", TaskState::Queued);
        let id = t.id;
        assert!(apply_offline(&store, &mut t, &DaemonCommand::Pause { task_id: id }).unwrap());
        assert_eq!(t.state, TaskState::Paused);
        assert!(apply_offline(&store, &mut t, &DaemonCommand::Resume { task_id: id }).unwrap());
        assert_eq!(t.state, TaskState::Queued);
        assert!(apply_offline(&store, &mut t, &DaemonCommand::Cancel { task_id: id }).unwrap());
        assert_eq!(t.state, TaskState::Cancelled);
        assert!(apply_offline(&store, &mut t, &DaemonCommand::Retry { task_id: id }).unwrap());
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(store.get_task(id).unwrap().unwrap().state, TaskState::Queued);
        let mut na = stored(&store, "NA", TaskState::NeedsAttention);
        let na_id = na.id;
        assert!(apply_offline(&store, &mut na, &DaemonCommand::Resume { task_id: na_id }).unwrap());
        assert_eq!(na.state, TaskState::Queued, "a human may re-queue a task that needed attention");
        assert!(!apply_offline(&store, &mut na, &DaemonCommand::SyncNow).unwrap());
    }

    #[test]
    fn model_arg_parsing() {
        assert_eq!(parse_model_arg("auto").unwrap(), None);
        assert_eq!(parse_model_arg("Opus").unwrap(), Some(ModelTier::opus()));
        assert_eq!(parse_model_arg("gpt-6.1-sol").unwrap(), Some(ModelTier::new("gpt-6.1-sol")));
        assert!(parse_model_arg("turbo").is_err());
        assert!(parse_model_arg("gpt").is_err());
    }

    #[test]
    fn explain_lists_pending_blockers_and_children() {
        use crate::domain::LinkedIssue;
        let linked = |key: &str, state_type: &str, pr_merged: bool| LinkedIssue {
            key: key.into(),
            title: format!("T {key}"),
            state_type: state_type.into(),
            pr_merged,
        };
        let mut t = Task::new("A-1", "a", crate::domain::TaskSource::Manual);
        assert!(dependency_lines(&t).is_empty());
        t.blocked_by = vec![linked("B-1", "started", false), linked("B-2", "started", true), linked("B-3", "completed", false)];
        t.parent = Some("P-1".into());
        let text = dependency_lines(&t).join("\n");
        assert!(text.contains("waiting on B-1: not scheduled"), "{text}");
        assert!(text.contains("B-1 T B-1 (pending, started)"), "{text}");
        assert!(text.contains("B-2 T B-2 (satisfied, PR merged)"), "{text}");
        assert!(text.contains("B-3 T B-3 (satisfied, completed)"), "{text}");
        assert!(text.contains("sub-issue of P-1"), "{text}");

        t.blocked_by.clear();
        t.children = vec![linked("C-1", "completed", false), linked("C-2", "unstarted", false)];
        let text = dependency_lines(&t).join("\n");
        assert!(text.contains("parent of 2 sub-issue(s): never scheduled; waiting on C-2"), "{text}");
        t.children[1].state_type = "canceled".into();
        assert!(dependency_lines(&t).join("\n").contains("all done; the daemon closes it"));
        t.children[0].state_type = "canceled".into();
        assert!(dependency_lines(&t).join("\n").contains("all canceled: powerqueue leaves it to you"));
    }
}
