//! Pure transitions for user commands (`task pause|resume|cancel|retry|model`).
//!
//! Each function mutates the in-memory [`Task`] (and the live [`Session`],
//! when one is handed in) and returns the [`Effect`]s the daemon must carry
//! out: events to log, a window to kill, a cleanup, a relayed answer to
//! forget. A command that does not apply to the task's state leaves it
//! untouched and returns at most a log effect; the daemon persists a task
//! only when it changed.

use chrono::{DateTime, Utc};

use crate::domain::{EventLevel, ModelTier, Session, SessionState, Task, TaskState};

use super::review;
use super::transitions::{Effect, SKIP_REASON};

/// `task pause`. `live` says whether the task has a live session (it
/// finishes its turn and is not relaunched). Terminal and already paused
/// tasks are left alone.
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
/// A PRIORITY.md skip marker is cleared. Other states are left alone.
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
        task.last_error = None;
        task.summary = None;
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
/// the session with it). Returns its attempt number; `None` when there is
/// no live session.
fn end_live(session: Option<&mut Session>, now: DateTime<Utc>, effects: &mut Vec<Effect>) -> Option<u32> {
    let session = session.filter(|s| s.state.is_live())?;
    session.state = SessionState::Killed;
    session.ended_at = Some(now);
    effects.push(Effect::KillWindow);
    Some(session.attempt)
}

/// `task model <tier>` / `task model --clear`: the override for the next
/// attempt (the policy may still downgrade it).
pub fn on_set_model(task: &mut Task, model: Option<ModelTier>) -> Vec<Effect> {
    task.model_override = model.clone();
    let msg = match &model {
        Some(m) => format!("model forced to {m} for the next attempt"),
        None => "model override cleared".to_string(),
    };
    vec![Effect::log(EventLevel::Info, "task.model_override", msg, serde_json::json!({ "model": model }))]
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
            assert!(on_pause(&mut t, false).is_empty());
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
        assert!(on_resume(&mut t, true, now()).is_empty());
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
        assert!(on_cancel(&mut t, None, now()).is_empty());
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
            let mut t = task.clone();
            let mut s = session.clone();
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
            if !matches!(task.state, TaskState::Paused | TaskState::NeedsAttention) {
                prop_assert!(on_resume(&mut t, live, now).is_empty());
                prop_assert_eq!(&t, &task);
            } else {
                on_resume(&mut t, live, now);
                prop_assert_eq!(t.not_before, None);
                prop_assert!(matches!(t.state, TaskState::Running | TaskState::InReview | TaskState::Queued));
                prop_assert_eq!(t.state == TaskState::Running, live);
            }
        }

        /// Effects are justified: a window is killed iff the session handed
        /// in was live (and it is then closed); cleanup only for a task that
        /// ended; a relayed answer is forgotten only by a re-queue.
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
            if was_live && !task.state.is_terminal() {
                prop_assert_eq!((s.state, s.ended_at), (SessionState::Killed, Some(now)));
            } else {
                prop_assert_eq!(&s, &session);
            }
            // Retry.
            let mut t = task.clone();
            let mut s = session.clone();
            let effects = on_retry(&mut t, Some(&mut s), now);
            let applies = task.state.is_terminal()
                || matches!(task.state, TaskState::Crashed | TaskState::Throttled | TaskState::Paused | TaskState::NeedsAttention)
                || (task.state == TaskState::InReview && task.pr_url.as_deref().and_then(pr_number_of).is_some());
            prop_assert_eq!(has_kill(&effects), applies && was_live);
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
