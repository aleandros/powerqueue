//! Pure transitions for user commands (`task pause|resume|cancel|retry|model|mode`)
//! and the direct writes (`task complete`, `task complete --pr`, `task block`).
//!
//! Each function mutates the in-memory [`Task`] (and the live [`Session`],
//! when one is handed in) and returns the [`Effect`]s the daemon must carry
//! out: events to log, a window to kill, a cleanup, a relayed answer to
//! forget. A command that does not apply to the task's state leaves it
//! untouched and returns at most a log effect; the daemon persists a task
//! only when it changed. A direct write that does not apply is [`Refused`]
//! with the message the CLI shows.

use chrono::{DateTime, Utc};

use crate::config::{ClaudeSessionMode, Config};
use crate::domain::{EventLevel, ModelTier, Session, SessionState, Task, TaskState};
use crate::github::PrRef;

use super::review;
use super::transitions::{Effect, SKIP_REASON};

/// Why a direct write (`task complete`, `task block`) does not apply to the
/// task; shown to the user as is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Refused(pub String);

/// Set/change/clear a local deferral without changing task or session state.
/// Refuses an in-progress launch; active sessions otherwise continue untouched.
/// Terminal tasks retain the hold for an explicit retry.
pub fn on_defer(task: &mut Task, deferral: Option<crate::deferral::Deferral>) -> Result<Vec<Effect>, Refused> {
    refuse_while_starting(task, "change deferral for")?;
    if task.deferral == deferral {
        return Ok(Vec::new());
    }
    let previous = std::mem::replace(&mut task.deferral, deferral);
    Ok(vec![Effect::log(
        EventLevel::Info,
        "task.deferral_changed",
        match &task.deferral {
            Some(d) => format!("local deferral set until {}", d.until.to_rfc3339()),
            None => "local deferral cleared; other scheduling restrictions still apply".into(),
        },
        serde_json::json!({"previous": previous, "deferral": task.deferral}),
    )])
}

/// Audit changes in the effective source hold, including invalid directives.
/// Source metadata remains owned by sync; this pure comparison only emits logs.
pub fn source_deferral_events(before: &Task, after: &Task) -> Vec<Effect> {
    let old = before.source_deferral().map_err(|e| e.to_string());
    let new = after.source_deferral().map_err(|e| e.to_string());
    if old == new {
        return Vec::new();
    }
    vec![Effect::log(
        EventLevel::Info,
        "task.source_deferral_changed",
        match &new {
            Ok(Some(d)) => format!("{} deferral set until {}", after.source.kind(), d.until.to_rfc3339()),
            Ok(None) => "source deferral cleared; local holds and other scheduling restrictions still apply".into(),
            Err(error) => format!("source deferral invalid; future launches held: {error}"),
        },
        serde_json::json!({"source": after.source, "previous": old, "deferral": new}),
    )]
}

/// Change an unstarted task's project after the caller validates the target.
/// Refuses live, previously bound or artifact-bearing tasks: moving existing
/// work requires a new task so transcripts and branches cannot cross repos.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "Keep the existing on_set_project flag interface during lint adoption; callers already supply named state predicates."
)]
pub fn on_set_project(task: &mut Task, project: &str, has_session: bool) -> Result<Vec<Effect>, Refused> {
    if has_session
        || task.execution_repo.is_some()
        || task.attempts > 0
        || task.started_at.is_some()
        || task.branch.is_some()
        || task.worktree_path.is_some()
        || task.review.is_some()
        || task.pr_url.is_some()
        || !matches!(
            task.state,
            TaskState::Queued | TaskState::Paused | TaskState::Blocked | TaskState::Throttled | TaskState::NeedsAttention
        )
    {
        return Err(Refused(format!(
            "cannot reassign {} after execution started; create a new task in the target project",
            task.key
        )));
    }
    let assignment = crate::routing::ProjectAssignment::Explicit { project: project.into() };
    if task.execution_project == project && task.project_assignment.as_ref() == Some(&assignment) {
        return Ok(Vec::new());
    }
    let old = std::mem::replace(&mut task.execution_project, project.to_string());
    task.project_assignment = Some(assignment);
    task.routing = None;
    Ok(vec![Effect::log(
        EventLevel::Info,
        "task.project_set",
        format!("execution project changed from {old} to {project}"),
        serde_json::json!({"from": old, "project": project}),
    )])
}

/// Reconcile current automatic routing advice for unstarted work, withholding unresolved replacements.
/// Explicit human selections, stale input and any session/history refuse the change.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "Keep the existing on_analysis_project flag interface during lint adoption; callers already supply named state predicates."
)]
pub fn on_analysis_project(
    task: &mut Task,
    cfg: &crate::config::Config,
    record: &crate::analysis::AnalysisRecord,
    has_session: bool,
) -> Vec<Effect> {
    // Evaluate eligibility without the previous automatic decision: a new result
    // supersedes that evidence even when its input fingerprint is unchanged.
    let mut candidate = task.clone();
    let replacing = matches!(candidate.project_assignment, Some(crate::routing::ProjectAssignment::Automatic { .. }));
    if replacing {
        candidate.project_assignment = None;
    }
    if !cfg.needs_project_analysis(&candidate) || record.stale(cfg, task) || has_session {
        return Vec::new();
    }
    let Ok(input) = crate::analysis::AnalysisInput::new(cfg, task) else { return Vec::new() };
    let project = record
        .proposal
        .clone()
        .and_then(|proposal| proposal.validate(&input, cfg.analysis.min_confidence).ok())
        .and_then(|proposal| proposal.project);
    let Some(project) = project else {
        if !replacing || on_set_project(&mut candidate, "", false).is_err() {
            return Vec::new();
        }
        let from = task.execution_project.clone();
        candidate.project_assignment = None;
        *task = candidate;
        let mut effects = vec![Effect::log(
            EventLevel::Info,
            "routing.automatic_withheld",
            "Replacement analysis is unresolved; repository selection required",
            serde_json::json!({"from": from, "fingerprint": record.fingerprint}),
        )];
        effects.extend(super::transitions::on_routing(task, cfg.route(task), false));
        return effects;
    };
    let assignment =
        crate::routing::ProjectAssignment::Automatic { project: project.clone(), fingerprint: record.fingerprint.clone() };
    if task.execution_project == project && task.project_assignment.as_ref() == Some(&assignment) {
        return Vec::new();
    }
    let from = task.execution_project.clone();
    if on_set_project(task, &project, false).is_err() {
        return Vec::new();
    }
    task.project_assignment = Some(assignment);
    let mut effects = vec![Effect::log(
        EventLevel::Info,
        "routing.automatic_assigned",
        format!("automatically selected repository {project}"),
        serde_json::json!({"from": from, "project": project, "confidence": record.proposal.as_ref().map(|p| p.confidence), "fingerprint": record.fingerprint}),
    )];
    let decision = cfg.route(task);
    effects.extend(super::transitions::on_routing(task, decision, false));
    effects
}

/// Pin the checkout before the first repository operation. Refuses a changed
/// checkout on retries and reviews. The shell supplies the canonical path.
pub fn bind_project(task: &mut Task, checkout: &str) -> Result<Vec<Effect>, Refused> {
    if let Some(bound) = &task.execution_repo {
        if bound != checkout {
            return Err(Refused(format!("task {} is pinned to {bound}, not {checkout}", task.key)));
        }
        return Ok(Vec::new());
    }
    task.execution_repo = Some(checkout.into());
    Ok(vec![Effect::log(
        EventLevel::Info,
        "task.project_bound",
        format!("execution project {} pinned to {checkout}", task.execution_project),
        serde_json::json!({"project": task.execution_project, "checkout": checkout}),
    )])
}

/// `task complete` without a PR, written straight into the store by the
/// agent from inside its session or by a human: the task goes `completed`
/// with `summary` and the daemon releases its session, if still live, on
/// its next tick ([`super::transitions::release_session`]). Refused for a
/// task that is already completed or that the table does not let complete
/// ([`TaskState::can_transition_to`]), and for a `starting` one: the daemon
/// is launching it and persists `running` when the launch ends, which would
/// lose the completion.
pub fn on_complete(task: &mut Task, summary: Option<&str>, now: DateTime<Utc>) -> Result<Vec<Effect>, Refused> {
    if task.leaves_ticket_open() && task.execution.as_ref().is_none_or(|e| e.outcome.is_none()) {
        return Err(Refused("analysis completion requires --outcome and --summary or --summary-file".into()));
    }
    if task.state == TaskState::Completed {
        return Err(Refused(format!("{} is already completed", task.key)));
    }
    refuse_while_starting(task, "complete")?;
    if !task.state.can_transition_to(TaskState::Completed) {
        return Err(Refused(format!("cannot complete {} while it is {} (cancel it instead)", task.key, task.state)));
    }
    task.state = TaskState::Completed;
    task.completed_at = Some(now);
    task.not_before = None;
    if let Some(s) = non_empty(summary) {
        task.summary = Some(s.to_string());
    }
    task.last_error = None;
    Ok(vec![Effect::log(
        EventLevel::Info,
        "task.completed_by_command",
        "marked completed via `powerqueue task complete`",
        serde_json::json!({ "summary": task.summary }),
    )])
}

/// Complete an analysis pass with a durable recommendation. No external effects run here.
/// Refuses missing summaries, ordinary coding tasks, or invalid completion states without mutation.
pub fn on_triage_complete(
    task: &mut Task,
    summary: &str,
    outcome: crate::execution::TriageOutcome,
    now: DateTime<Utc>,
) -> Result<Vec<Effect>, Refused> {
    if !task.leaves_ticket_open() || summary.trim().is_empty() {
        return Err(Refused("--outcome requires a leave-ticket-open workflow and a nonempty summary".into()));
    }
    let mut updated = task.clone();
    if let Some(execution) = &mut updated.execution {
        execution.outcome = Some(outcome);
    }
    let mut effects = on_complete(&mut updated, Some(summary), now)?;
    effects.push(Effect::log(
        EventLevel::Info,
        "triage.completed",
        "analysis finished; source issue remains open",
        serde_json::json!({"outcome": outcome, "queue": updated.execution.as_ref().map(|e| &e.queue)}),
    ));
    *task = updated;
    Ok(effects)
}

/// `task complete --pr <url>`: the task moves to `in_review` with the PR
/// recorded ([`Task::hand_off_for_review`]), so the daemon releases its
/// session, slot and worktree (keeping the branch) and watches the PR.
/// Calling it again while in review re-arms the watch (e.g. after a review
/// round). Refused for a URL that is not a GitHub pull request, a task that
/// is not running (queued, finished, ...) and a `starting` one (see
/// [`on_complete`]).
pub fn on_hand_off(task: &mut Task, pr_url: &str, summary: Option<&str>, now: DateTime<Utc>) -> Result<Vec<Effect>, Refused> {
    if task.leaves_ticket_open() {
        return Err(Refused("analysis workflows cannot hand off a PR; create a separate coding task".into()));
    }
    let pr: PrRef = pr_url.parse().map_err(Refused)?;
    refuse_while_starting(task, "hand off for review")?;
    if task.state != TaskState::InReview && !task.state.can_transition_to(TaskState::InReview) {
        return Err(Refused(format!(
            "cannot hand {} off for review while it is {} (only a running task opens a PR)",
            task.key, task.state
        )));
    }
    let from = task.state;
    task.hand_off_for_review(pr_url, now);
    if let Some(s) = non_empty(summary) {
        task.summary = Some(s.to_string());
    }
    let rounds = task.review.as_ref().map_or(0, |r| r.rounds);
    Ok(vec![Effect::log(
        EventLevel::Info,
        "task.in_review",
        format!(
            "handed off for review: {pr} (merge armed{})",
            if rounds > 0 { format!(", after {rounds} review round(s)") } else { String::new() }
        ),
        serde_json::json!({ "pr": task.pr_url, "from": from, "rounds": rounds, "summary": task.summary }),
    )])
}

/// `task block`: the task needs a human (`needs_attention`) for `reason`;
/// a live session keeps running (the agent is waiting for the answer).
/// Refused for a finished task, one the table does not let block, and a
/// `starting` one (see [`on_complete`]).
pub fn on_block(task: &mut Task, reason: Option<&str>) -> Result<Vec<Effect>, Refused> {
    if task.state.is_terminal() {
        return Err(Refused(format!("cannot block {}: it is already {}", task.key, task.state)));
    }
    refuse_while_starting(task, "block")?;
    if !task.state.can_transition_to(TaskState::NeedsAttention) {
        return Err(Refused(format!("cannot block {} while it is {}", task.key, task.state)));
    }
    task.state = TaskState::NeedsAttention;
    // A throttled task's retry time is over: a human decides when it runs.
    task.not_before = None;
    if let Some(r) = non_empty(reason) {
        task.last_error = Some(r.to_string());
    }
    Ok(vec![Effect::log(
        EventLevel::Warn,
        "task.blocked_by_command",
        format!("blocked via `powerqueue task block`: {}", task.last_error.as_deref().unwrap_or("no reason given")),
        serde_json::json!({ "reason": task.last_error }),
    )])
}

fn refuse_while_starting(task: &Task, verb: &str) -> Result<(), Refused> {
    if task.state == TaskState::Starting {
        return Err(Refused(format!(
            "cannot {verb} {} while the daemon is launching it; try again once it is running",
            task.key
        )));
    }
    Ok(())
}

fn non_empty(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|s| !s.is_empty())
}

/// `task pause`. `live` says whether the task has a live session (it
/// finishes its turn and is not relaunched). Terminal and already paused
/// tasks are left alone.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "Keep the existing on_pause flag interface during lint adoption; callers already supply named state predicates."
)]
pub fn on_pause(task: &mut Task, live: bool) -> Vec<Effect> {
    if task.state.is_terminal() || task.state == TaskState::Paused {
        return Vec::new();
    }
    task.state = TaskState::Paused;
    task.not_before = None;
    let msg = if live { "paused; the current session finishes its turn and will not be relaunched" } else { "paused" };
    vec![Effect::log(EventLevel::Info, "task.paused", msg, serde_json::json!({}))]
}

/// `task resume` of a paused or `needs_attention` task: back to `running`
/// when its session is still alive (`live`), to `in_review` when it was
/// parked by the PR watcher (with a fresh staleness clock), else `queued`.
/// A priority.toml skip marker is cleared. Other states are left alone.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "Keep the existing on_resume flag interface during lint adoption; callers already supply named state predicates."
)]
pub fn on_resume(task: &mut Task, live: bool, now: DateTime<Utc>) -> Vec<Effect> {
    if !matches!(task.state, TaskState::Paused | TaskState::NeedsAttention) {
        return Vec::new();
    }
    task.not_before = None;
    if task.last_error.as_deref() == Some(SKIP_REASON) {
        task.last_error = None;
    }
    task.state = if live {
        TaskState::Running
    } else if task.parked_in_review() {
        rearm_watch(task, now);
        TaskState::InReview
    } else {
        TaskState::Queued
    };
    vec![Effect::log(EventLevel::Info, "task.resumed", format!("resumed → {}", task.state), serde_json::json!({}))]
}

/// `task cancel`: the live session (if any) is killed and its row closed,
/// the task goes `cancelled` and its resources are released. Terminal tasks
/// are left alone.
pub fn on_cancel(task: &mut Task, session: Option<&mut Session>, now: DateTime<Utc>) -> Vec<Effect> {
    if task.state.is_terminal() {
        return Vec::new();
    }
    let mut effects = Vec::new();
    end_live(session, now, &mut effects);
    task.state = TaskState::Cancelled;
    task.completed_at = Some(now);
    task.not_before = None;
    effects.push(Effect::log(EventLevel::Info, "task.cancelled", "cancelled by user", serde_json::json!({})));
    effects.push(Effect::Cleanup { succeeded: false });
    effects
}

/// `task retry`. A terminal task starts over (attempts, error, summary and
/// timestamps reset); a crashed, throttled, paused or `needs_attention`
/// task is re-queued; a task in review gets a user review round
/// ([`review::request_round`]). A live session is ended first (it would
/// keep the slot). Other states only log why the retry was ignored. A
/// re-queue forgets any relayed answer ([`Effect::ForgetAnswer`]): a retry
/// starts over.
pub fn on_retry(task: &mut Task, session: Option<&mut Session>, now: DateTime<Utc>) -> Vec<Effect> {
    let review_round = task.state == TaskState::InReview;
    if review_round {
        if task.pr_url.as_deref().and_then(review::pr_number_of).is_none() {
            return vec![Effect::log(
                EventLevel::Warn,
                "task.retry_ignored",
                "retry ignored: the task is in review but its PR number is unknown",
                serde_json::json!({ "pr": task.pr_url }),
            )];
        }
    } else if task.state.is_terminal() {
        task.attempts = 0;
        task.free_attempts = 0;
        task.last_error = None;
        task.summary = None;
        if let Some(execution) = &mut task.execution {
            execution.outcome = None;
        }
        task.completed_at = None;
        task.started_at = None;
    } else if !matches!(task.state, TaskState::Crashed | TaskState::Throttled | TaskState::Paused | TaskState::NeedsAttention) {
        // Debug: the CLI also applies a retry itself when no daemon runs,
        // and the daemon drains it later.
        return vec![Effect::log(
            EventLevel::Debug,
            "task.retry_ignored",
            format!("retry ignored: the task is {}", task.state),
            serde_json::json!({ "state": task.state }),
        )];
    }
    let mut effects = Vec::new();
    // A retry is a fresh attempt: a session that is still alive (e.g. the
    // agent reported a blocker and is waiting) is ended first, otherwise it
    // would keep the slot and the task could never relaunch.
    if let Some(attempt) = end_live(session, now, &mut effects) {
        effects.push(Effect::log(
            EventLevel::Info,
            "session.ended",
            "live session ended by retry",
            serde_json::json!({ "attempt": attempt }),
        ));
    }
    if review_round {
        effects.extend(review::request_round(task, now));
        return effects;
    }
    task.state = TaskState::Queued;
    task.not_before = None;
    effects.push(Effect::ForgetAnswer);
    effects.push(Effect::log(EventLevel::Info, "task.retried", "re-queued by user", serde_json::json!({})));
    effects
}

/// End the live session a command must not leave behind (it would keep the
/// slot): `killed`, ended now, its window to be killed (the daemon forgets
/// the session with it). A `stopped` headless session is marked `killed`
/// too, so the next attempt starts over instead of resuming it. Returns its
/// attempt number; `None` when there is no live or stopped session.
fn end_live(session: Option<&mut Session>, now: DateTime<Utc>, effects: &mut Vec<Effect>) -> Option<u32> {
    let session = session.filter(|s| s.state.is_live() || s.state == SessionState::Stopped)?;
    if session.state.is_live() {
        session.ended_at = Some(now);
        effects.push(Effect::KillWindow);
    }
    session.state = SessionState::Killed;
    Some(session.attempt)
}

/// `task model <tier>` / `task model --clear`: the override for the next
/// attempt (the policy may still downgrade it).
#[expect(
    clippy::needless_pass_by_value,
    reason = "Keep the established ownership-taking boundary for callers and serialized inputs."
)]
pub fn on_set_model(task: &mut Task, model: Option<ModelTier>) -> Vec<Effect> {
    task.model_override.clone_from(&model);
    let msg = match &model {
        Some(m) => format!("model forced to {m} for the next attempt"),
        None => "model override cleared".to_string(),
    };
    vec![Effect::log(EventLevel::Info, "task.model_override", msg, serde_json::json!({ "model": model }))]
}

/// `task mode <interactive|headless>` / `task mode auto`: the task's own
/// session-mode override, which beats every rule. A task that has started
/// gets its mode chosen again now under `cfg` (its project's configuration):
/// the override, or with `auto` the rules. One that has not started chooses
/// at its first start. A live session keeps running in its mode until it
/// ends; the next start uses the new one. Nothing else changes.
pub fn on_set_session_mode(task: &mut Task, mode: Option<ClaudeSessionMode>, cfg: &Config) -> Vec<Effect> {
    let previous = task.session_mode.take();
    task.session_mode_override = mode;
    if task.attempts > 0 || task.started_at.is_some() {
        task.session_mode = Some(cfg.resolve_session_mode(task));
    }
    let msg = match (mode, &task.session_mode) {
        (Some(m), _) => format!("session mode forced to {m} from the next start"),
        (None, Some(chosen)) => format!("session mode override cleared; the next start runs {chosen}"),
        (None, None) => "session mode override cleared; the rules choose at the first start".to_string(),
    };
    vec![Effect::log(
        EventLevel::Info,
        "task.session_mode_override",
        msg,
        serde_json::json!({ "mode": mode, "previous": previous, "session_mode": task.session_mode }),
    )]
}

/// Watch the PR again from `now` (see [`crate::domain::ReviewWatch::rearm`]).
pub fn rearm_watch(task: &mut Task, now: DateTime<Utc>) {
    if let Some(watch) = task.review.as_mut() {
        watch.rearm(now);
    }
    task.last_error = None;
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::domain::{ReviewWatch, TaskSource};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn task(state: TaskState) -> Task {
        let mut t = Task::new("ENG-1", "t", TaskSource::Manual);
        t.state = state;
        t.attempts = 2;
        t.branch = Some("pq/eng-1".into());
        t
    }

    fn session(task: &Task, state: SessionState) -> Session {
        Session {
            id: uuid::Uuid::new_v4(),
            task_id: task.id,
            attempt: 2,
            model: ModelTier::new("sonnet"),
            state,
            tmux_session: "powerqueue".into(),
            tmux_window: "@1".into(),
            pane_id: Some("%1".into()),
            pid: Some(42),
            transcript_path: None,
            exit_code: None,
            started_at: now() - Duration::minutes(10),
            ended_at: None,
            last_activity_at: now() - Duration::minutes(1),
            error: None,
            agent_session_id: None,
            waiting_since: None,
            waited_secs: 0,
            runtime: crate::domain::SessionRuntime::Tmux,
        }
    }

    fn kinds(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .map(|e| match e {
                Effect::Log { kind, .. } => kind.clone(),
                other => format!("{other:?}").split(' ').next().unwrap_or_default().trim_end_matches('{').to_string(),
            })
            .collect()
    }

    #[test]
    fn pause_only_applies_to_open_tasks() {
        let mut t = task(TaskState::Running);
        let effects = on_pause(&mut t, true);
        assert_eq!(t.state, TaskState::Paused);
        assert!(matches!(&effects[..], [Effect::Log { message, .. }] if message.contains("finishes its turn")));
        let mut t = task(TaskState::Queued);
        t.not_before = Some(now());
        assert!(matches!(&on_pause(&mut t, false)[..], [Effect::Log { message, .. }] if message == "paused"));
        assert_eq!(t.not_before, None);
        for state in [TaskState::Completed, TaskState::Failed, TaskState::Cancelled, TaskState::Paused] {
            let mut t = task(state);
            assert_eq!((on_pause(&mut t, false)).len(), 0);
            assert_eq!(t.state, state);
        }
    }

    #[test]
    fn resume_goes_where_the_task_came_from() {
        let mut t = task(TaskState::Paused);
        t.last_error = Some(SKIP_REASON.into());
        assert_eq!(kinds(&on_resume(&mut t, true, now())), ["task.resumed"]);
        assert_eq!((t.state, t.last_error.clone()), (TaskState::Running, None));

        let mut t = task(TaskState::NeedsAttention);
        on_resume(&mut t, false, now());
        assert_eq!(t.state, TaskState::Queued);

        let mut t = task(TaskState::NeedsAttention);
        t.pr_url = Some("https://github.com/o/r/pull/7".into());
        let mut watch = ReviewWatch::armed(now() - Duration::hours(3), None);
        watch.parked = true;
        watch.fingerprint = Some("x".into());
        t.review = Some(watch);
        t.last_error = Some("PR closed".into());
        on_resume(&mut t, false, now());
        assert_eq!(t.state, TaskState::InReview);
        let watch = t.review.unwrap();
        assert!(!watch.parked && watch.fingerprint.is_none() && watch.last_change_at == now(), "watch re-armed");
        assert_eq!(t.last_error, None);

        let mut t = task(TaskState::Running);
        assert_eq!((on_resume(&mut t, true, now())).len(), 0);
    }

    #[test]
    fn cancel_kills_the_live_session_and_cleans_up() {
        let mut t = task(TaskState::Running);
        let mut s = session(&t, SessionState::Running);
        let effects = on_cancel(&mut t, Some(&mut s), now());
        assert_eq!(kinds(&effects), ["KillWindow", "task.cancelled", "Cleanup"]);
        assert_eq!(t.state, TaskState::Cancelled);
        assert_eq!(t.completed_at, Some(now()));
        assert_eq!((s.state, s.ended_at), (SessionState::Killed, Some(now())));

        let mut t = task(TaskState::Queued);
        let mut s = session(&t, SessionState::Exited);
        let effects = on_cancel(&mut t, Some(&mut s), now());
        assert_eq!(kinds(&effects), ["task.cancelled", "Cleanup"], "a dead session is not touched");
        assert_eq!(s.state, SessionState::Exited);

        let mut t = task(TaskState::Completed);
        assert_eq!((on_cancel(&mut t, None, now())).len(), 0);
    }

    #[test]
    fn retry_starts_terminal_tasks_over_and_requeues_stuck_ones() {
        let mut t = task(TaskState::Failed);
        t.last_error = Some("boom".into());
        t.summary = Some("s".into());
        t.completed_at = Some(now());
        let effects = on_retry(&mut t, None, now());
        assert_eq!(kinds(&effects), ["ForgetAnswer", "task.retried"]);
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!((t.attempts, t.last_error.clone(), t.summary.clone(), t.completed_at), (0, None, None, None));

        let mut t = task(TaskState::NeedsAttention);
        let mut s = session(&t, SessionState::Idle);
        let effects = on_retry(&mut t, Some(&mut s), now());
        assert_eq!(kinds(&effects), ["KillWindow", "session.ended", "ForgetAnswer", "task.retried"]);
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.attempts, 2, "a stuck task keeps its attempt count");
        assert_eq!(s.state, SessionState::Killed);

        let mut t = task(TaskState::Running);
        let effects = on_retry(&mut t, None, now());
        assert!(matches!(&effects[..], [Effect::Log { level: EventLevel::Debug, kind, .. }] if kind == "task.retry_ignored"));
        assert_eq!(t.state, TaskState::Running);
    }

    #[test]
    fn retry_in_review_requests_a_round() {
        let mut t = task(TaskState::InReview);
        t.pr_url = Some("https://github.com/o/r/pull/7".into());
        t.review = Some(ReviewWatch::armed(now(), None));
        let effects = on_retry(&mut t, None, now());
        assert_eq!(kinds(&effects), ["review.relaunch"]);
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.review.as_ref().and_then(|w| w.relaunch.as_ref()).map(|r| r.pr_number), Some(7));

        let mut t = task(TaskState::InReview);
        t.pr_url = Some("not a pr".into());
        let effects = on_retry(&mut t, None, now());
        assert!(matches!(&effects[..], [Effect::Log { level: EventLevel::Warn, kind, .. }] if kind == "task.retry_ignored"));
        assert_eq!(t.state, TaskState::InReview);
    }

    #[test]
    fn set_model_records_the_override() {
        let mut t = task(TaskState::Queued);
        on_set_model(&mut t, Some(ModelTier::new("fable")));
        assert_eq!(t.model_override, Some(ModelTier::new("fable")));
        let effects = on_set_model(&mut t, None);
        assert!(matches!(&effects[..], [Effect::Log { message, .. }] if message == "model override cleared"));
        assert_eq!(t.model_override, None);
    }
}

#[cfg(test)]
mod properties {
    use proptest::prelude::*;

    use super::*;
    use crate::scheduler::review::pr_number_of;
    use crate::strategies::*;

    fn logs_are_well_formed(effects: &[Effect]) -> bool {
        effects.iter().all(|e| !matches!(e, Effect::Log { kind, message, .. } if kind.is_empty() || message.is_empty()))
    }

    fn has_kill(effects: &[Effect]) -> bool {
        effects.iter().any(|e| matches!(e, Effect::KillWindow))
    }

    proptest! {
        #[test]
        fn triage_completion_is_local_and_retry_keeps_workflow(mut task in crate::strategies::triage_task(), summary in "[a-zA-Z0-9 ]{1,100}") {
            let before = task.clone();
            let effects = on_triage_complete(&mut task, &format!("Findings: {summary}"), crate::execution::TriageOutcome::NeedsInformation, origin()).unwrap();
            prop_assert_eq!(task.state, TaskState::Completed);
            prop_assert!(task.leaves_ticket_open());
            prop_assert_eq!(&task.source, &before.source);
            prop_assert!(effects.iter().all(|e| matches!(e, Effect::Log { .. })), "completion only logs local state");
            prop_assert_eq!(task.execution.as_ref().unwrap().outcome, Some(crate::execution::TriageOutcome::NeedsInformation));
            on_retry(&mut task, None, origin());
            prop_assert_eq!(task.state, TaskState::Queued);
            prop_assert!(task.leaves_ticket_open());
            prop_assert_eq!(task.execution.as_ref().unwrap().outcome, None);
        }
    }

    proptest! {
        #[test]
        fn project_binding_is_stable_and_reassignment_is_explicit(mut task in crate::strategies::task(), project in crate::strategies::execution_project()) {
            let original_project = task.execution_project.clone();
            let source = task.source.clone();
            let linear_project = task.project.clone();
            let checkout = format!("/repos/{original_project}");
            bind_project(&mut task, &checkout).unwrap();
            let bound = task.clone();
            prop_assert!(bind_project(&mut task, "/another/repo").is_err());
            prop_assert!(on_set_project(&mut task, &project, false).is_err());
            prop_assert_eq!(&task, &bound);
            prop_assert_eq!(&task.source, &source);
            prop_assert_eq!(&task.project, &linear_project);
            prop_assert!(bind_project(&mut task, &checkout).unwrap().is_empty());
        }

        #[test]
        fn automatic_selection_cannot_change_approval_or_retarget_history(
            mut task in crate::strategies::task(), proposal in crate::strategies::analysis_proposal(),
            session in any::<bool>(), explicit in any::<bool>(), stale in any::<bool>(), replacing in any::<bool>()
        ) {
            let mut cfg = crate::config::Config::default();
            cfg.repo.path = "/repos/default".into();
            cfg.routing.mode = crate::routing::RoutingMode::Automatic;
            cfg.analysis.enabled = true;
            cfg.analysis.projects.insert("default".into(), "Application".into());
            task.source = crate::domain::TaskSource::Manual;
            if explicit { task.project_assignment = Some(crate::routing::ProjectAssignment::Explicit { project: "default".into() }); }
            let input = crate::analysis::AnalysisInput::new(&cfg, &task).unwrap();
            let record = crate::analysis::AnalysisRecord { created_at: Utc::now(), fingerprint: input.fingerprint(&cfg), proposal: Some(proposal), attempts: vec![] };
            if stale { task.description.push_str(" changed"); }
            if replacing && !explicit {
                task.project_assignment = Some(crate::routing::ProjectAssignment::Automatic { project: "default".into(), fingerprint: record.fingerprint.clone() });
            }
            let before = task.clone();
            let effects = on_analysis_project(&mut task, &cfg, &record, session);
            if session || explicit || stale || crate::routing::has_execution_history(&before) {
                prop_assert_eq!(&task, &before);
                prop_assert!(effects.is_empty());
            }
            if !effects.is_empty() {
                let accepted = record.proposal.clone().unwrap().validate(&input, cfg.analysis.min_confidence).ok().and_then(|p| p.project);
                prop_assert_eq!(accepted.as_deref().unwrap_or(""), task.execution_project.as_str());
                prop_assert_eq!(cfg.route(&task).permitted, accepted.is_some());
                let mut expected = before;
                expected.execution_project = task.execution_project.clone();
                expected.project_assignment = task.project_assignment.clone();
                expected.routing = task.routing.clone();
                prop_assert_eq!(task, expected, "automatic routing changed unrelated task authority");
            }
        }

        #[test]
        fn fresh_project_reassignment_preserves_source(project in crate::strategies::execution_project()) {
            let mut task = Task::new("T", "t", crate::domain::TaskSource::Manual);
            task.project = Some("source project".into());
            let before = task.clone();
            let effects = on_set_project(&mut task, &project, false).unwrap();
            prop_assert_eq!(&task.execution_project, &project);
            prop_assert_eq!(&task, &Task { execution_project: project.clone(), project_assignment: Some(crate::routing::ProjectAssignment::Explicit {project: project.clone()}), routing: None, ..before });
            prop_assert!(!effects.is_empty());
            prop_assert!(on_set_project(&mut task, "other", true).is_err());
        }
    }

    proptest! {
        /// Every command makes a move the transition table allows.
        #[test]
        fn commands_make_legal_moves((task, session) in task_with_session(), live in any::<bool>(), model in prop::option::of(claude_model())) {
            let now = origin();
            for cmd in 0..5u8 {
                let before = task.clone();
                let mut t = task.clone();
                let mut s = session.clone();
                let effects = match cmd {
                    0 => on_pause(&mut t, live),
                    1 => on_resume(&mut t, live, now),
                    2 => on_cancel(&mut t, Some(&mut s), now),
                    3 => on_retry(&mut t, Some(&mut s), now),
                    _ => on_set_model(&mut t, model.clone()),
                };
                prop_assert!(before.state.can_transition_to(t.state), "cmd {cmd}: {:?} -> {:?}", before.state, t.state);
                prop_assert!(logs_are_well_formed(&effects));
            }
        }

        /// Terminal tasks are inert for pause and cancel; only retry leaves a
        /// terminal state, and then the task starts over.
        #[test]
        fn terminal_tasks_only_leave_through_retry(
            (task, session) in task_with_session_among(&[TaskState::Completed, TaskState::Failed, TaskState::Cancelled]),
            live in any::<bool>(),
        ) {
            let now = origin();
            let mut t = task.clone();
            prop_assert!(on_pause(&mut t, live).is_empty());
            prop_assert_eq!(&t, &task);
            let mut s = session.clone();
            prop_assert!(on_cancel(&mut t, Some(&mut s), now).is_empty());
            prop_assert_eq!(&t, &task);
            prop_assert_eq!(&s, &session);
            let mut t = task;
            let mut s = session;
            let effects = on_retry(&mut t, Some(&mut s), now);
            prop_assert_eq!(t.state, TaskState::Queued);
            prop_assert_eq!(t.attempts, 0);
            prop_assert_eq!((t.last_error.clone(), t.summary.clone(), t.completed_at, t.started_at, t.not_before), (None, None, None, None, None));
            prop_assert!(effects.iter().any(|e| matches!(e, Effect::ForgetAnswer)));
            prop_assert!(!has_kill(&effects), "a terminal task's session is already dead");
        }

        /// A second pause changes nothing and says nothing; resuming a task
        /// that is neither paused nor waiting for a human changes nothing.
        /// (A task that was already paused is left alone, retry time
        /// included: the generator may give it one, the daemon never does.)
        #[test]
        fn pause_and_resume_are_idempotent((task, _) in task_with_session(), live in any::<bool>()) {
            let now = origin();
            let mut t = task.clone();
            on_pause(&mut t, live);
            let paused = t.clone();
            prop_assert!(on_pause(&mut t, live).is_empty());
            prop_assert_eq!(&t, &paused);
            if !task.state.is_terminal() && task.state != TaskState::Paused {
                prop_assert_eq!((paused.state, paused.not_before), (TaskState::Paused, None));
            }
            let mut t = task.clone();
            if matches!(task.state, TaskState::Paused | TaskState::NeedsAttention) {
                on_resume(&mut t, live, now);
                prop_assert_eq!(t.not_before, None);
                prop_assert!(matches!(t.state, TaskState::Running | TaskState::InReview | TaskState::Queued));
                prop_assert_eq!(t.state == TaskState::Running, live);
            } else {
                prop_assert!(on_resume(&mut t, live, now).is_empty());
                prop_assert_eq!(&t, &task);
            }
        }

        /// Effects are justified: a window is killed iff the session handed
        /// in was live (and it is then closed); a stopped headless session
        /// is closed without a kill so it is never resumed; cleanup only for
        /// a task that ended; a relayed answer is forgotten only by a re-queue.
        #[test]
        fn cancel_and_retry_effects_are_justified((task, session) in task_with_session()) {
            let now = origin();
            let was_live = session.state.is_live();
            // Cancel.
            let mut t = task.clone();
            let mut s = session.clone();
            let effects = on_cancel(&mut t, Some(&mut s), now);
            if task.state.is_terminal() {
                prop_assert!(effects.is_empty());
            } else {
                prop_assert_eq!(has_kill(&effects), was_live);
                prop_assert_eq!((t.state, t.completed_at, t.not_before), (TaskState::Cancelled, Some(now), None));
                prop_assert!(effects.iter().any(|e| matches!(e, Effect::Cleanup { succeeded: false })), "no failed cleanup: {:?}", effects);
            }
            let was_stopped = session.state == SessionState::Stopped;
            if was_live && !task.state.is_terminal() {
                prop_assert_eq!((s.state, s.ended_at), (SessionState::Killed, Some(now)));
            } else if was_stopped && !task.state.is_terminal() {
                prop_assert_eq!(&s, &Session { state: SessionState::Killed, ..session.clone() });
            } else {
                prop_assert_eq!(&s, &session);
            }
            // Retry.
            let mut t = task.clone();
            let mut s = session;
            let effects = on_retry(&mut t, Some(&mut s), now);
            let applies = task.state.is_terminal()
                || matches!(task.state, TaskState::Crashed | TaskState::Throttled | TaskState::Paused | TaskState::NeedsAttention)
                || (task.state == TaskState::InReview && task.pr_url.as_deref().and_then(pr_number_of).is_some());
            prop_assert_eq!(has_kill(&effects), applies && was_live);
            if applies && was_stopped {
                prop_assert_eq!(s.state, SessionState::Killed, "a retry never resumes a stopped session");
            }
            prop_assert!(!effects.iter().any(|e| matches!(e, Effect::Cleanup { .. })), "retry never cleans up: {:?}", effects);
            let forgot = effects.iter().any(|e| matches!(e, Effect::ForgetAnswer));
            prop_assert_eq!(forgot, applies && task.state != TaskState::InReview);
            if applies {
                prop_assert_eq!((t.state, t.not_before), (TaskState::Queued, None));
            } else {
                prop_assert_eq!(&t, &task);
                prop_assert!(matches!(&effects[..], [Effect::Log { kind, .. }] if kind == "task.retry_ignored"), "{effects:?}");
            }
            if task.state == TaskState::InReview && applies {
                let watch = t.review.as_ref().expect("a watch");
                prop_assert_eq!(watch.relaunch.as_ref().map(|r| r.reason.as_str()), Some("requested"));
                prop_assert_eq!(watch.rounds, task.review.as_ref().map_or(0, |w| w.rounds));
                prop_assert!(!watch.parked);
            }
        }

        /// `task mode` records the override and chooses the mode again: now for
        /// a started task (the override first, else the rules), at the first
        /// start otherwise; nothing else, not even a live session, changes.
        #[test]
        fn set_session_mode_only_changes_the_override(
            (task, session) in task_with_session(),
            mode in prop::option::of(session_mode()),
            cfg in session_mode_config(),
        ) {
            let mut t = task.clone();
            let effects = on_set_session_mode(&mut t, mode, &cfg);
            prop_assert_eq!(t.session_mode_override, mode);
            let started = task.attempts > 0 || task.started_at.is_some();
            prop_assert_eq!(t.session_mode.is_some(), started);
            prop_assert_eq!(cfg.session_mode_for(&t), cfg.resolve_session_mode(&t));
            if let Some(mode) = mode {
                prop_assert_eq!(cfg.session_mode_for(&t).mode, mode);
            }
            t.session_mode_override = task.session_mode_override;
            t.session_mode.clone_from(&task.session_mode);
            prop_assert_eq!(&t, &task);
            prop_assert_eq!(session.task_id, task.id);
            prop_assert!(matches!(&effects[..], [Effect::Log { kind, .. }] if kind == "task.session_mode_override"), "{:?}", effects);
        }

        /// `task model` only records the override and logs it.
        #[test]
        fn set_model_only_changes_the_override((task, _) in task_with_session(), model in prop::option::of(model())) {
            let mut t = task.clone();
            let effects = on_set_model(&mut t, model.clone());
            prop_assert_eq!(t.model_override, model);
            t.model_override = task.model_override.clone();
            prop_assert_eq!(&t, &task);
            prop_assert!(matches!(&effects[..], [Effect::Log { kind, .. }] if kind == "task.model_override"), "{:?}", effects);
        }
    }
}
