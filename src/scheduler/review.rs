//! The PR watcher's decisions (pure).
//!
//! A task handed off with `powerqueue task complete --pr <url>` sits in
//! `in_review` without a session. Every `scheduler.pr_poll_secs` the daemon
//! asks GitHub for the PR ([`crate::github::Gh::pr_status`]) and
//! [`on_pr_status`] decides, in this order:
//!
//! | PR | result |
//! |----|--------|
//! | merged | `completed`; local branch deleted; Linear untouched |
//! | closed without merge | `needs_attention` |
//! | `CONFLICTING` | relaunch, reason `conflict` |
//! | a required check failed | relaunch, reason `ci_failed <names>` |
//! | dropped by the merge queue after the arming, not queued or armed again | relaunch, reason `ci_failed merge queue: <why>` |
//! | unresolved threads with comments newer than the arming | relaunch, reason `review` |
//! | labelled with the hold label | wait for a human to merge (no stale report) |
//! | unchanged for `review_stale_hours` | Linear comment + `needs_attention` |
//!
//! A relaunch moves the task back to `queued` with a pending
//! [`ReviewRelaunch`]; the scheduler then recreates the worktree at the same
//! path and resumes the same agent session with `scheduler.review_prompt`.
//! Relaunches count against `scheduler.review_rounds_max`, not `max_attempts`.

use chrono::{DateTime, Duration, Utc};

use crate::config::SchedulerConfig;
use crate::domain::{EventLevel, ReviewRelaunch, ReviewWatch, Task, TaskState};
use crate::github::PrStatus;

use super::transitions::Effect;

/// `last_error` of a task whose PR was closed without merging.
pub const CLOSED_UNMERGED: &str = "the pull request was closed without merging";

/// `last_error` prefix of a PR watcher failure (gh missing, no access, ...).
pub const WATCHER_ERROR_PREFIX: &str = "PR watcher:";

/// An `in_review` task whose `pr_url` is not a usable pull request: nothing
/// can be watched, so a human must look at it.
pub fn on_unusable_pr(task: &mut Task, error: &str) -> Vec<Effect> {
    let url = task.pr_url.clone().unwrap_or_default();
    park(task, format!("in review without a usable pull request: {error}"));
    vec![Effect::Log {
        level: EventLevel::Warn,
        kind: "review.error".into(),
        message: error.to_string(),
        data: serde_json::json!({ "pr": url }),
    }]
}

/// The PR could not be read (`gh` missing, no access, ...): the task is not
/// changed, the failure is noted in `last_error` (logged as `review.error`
/// only when the message changed) and the poll is retried at the next
/// interval.
pub fn on_watch_error(task: &mut Task, pr: &str, error: &str, now: DateTime<Utc>) -> Vec<Effect> {
    let message = format!("{WATCHER_ERROR_PREFIX} cannot read {pr}: {error}");
    let changed = task.last_error.as_deref() != Some(message.as_str());
    task.review.get_or_insert_with(|| ReviewWatch::armed(now, None)).last_polled_at = Some(now);
    task.last_error = Some(message.clone());
    if !changed {
        return Vec::new();
    }
    vec![Effect::Log {
        level: EventLevel::Warn,
        kind: "review.error".into(),
        message,
        data: serde_json::json!({ "pr": task.pr_url }),
    }]
}

/// The PR can be read again: a watcher error noted earlier is cleared.
pub fn on_watch_recovered(task: &mut Task) {
    if task.last_error.as_deref().is_some_and(|e| e.starts_with(WATCHER_ERROR_PREFIX)) {
        task.last_error = None;
    }
}

/// One-line summary of what GitHub reported; a change of this value is a
/// change of the PR (staleness restarts).
pub fn fingerprint(status: &PrStatus, armed_at: DateTime<Utc>) -> String {
    let failed = status.failed_required_checks();
    let finished = status.checks.iter().filter(|c| !c.conclusion.is_empty()).count();
    format!(
        "{} {} {} queued {} head {} checks {finished}/{} failed [{}] new threads {} labels [{}]",
        status.state,
        status.mergeable,
        status.merge_state_status,
        status.in_merge_queue,
        status.head_oid.as_deref().map(|h| &h[..h.len().min(7)]).unwrap_or("?"),
        status.checks.len(),
        failed.join(","),
        status.new_unresolved_threads(armed_at),
        status.labels.join(",")
    )
}

/// Human summary for the timeline (`review.status` events).
fn describe(status: &PrStatus, armed_at: DateTime<Utc>) -> String {
    let failed = status.failed_required_checks();
    let pending = status.checks.iter().filter(|c| c.conclusion.is_empty()).count();
    let mut parts = vec![format!("PR #{} {}", status.number, status.state.to_lowercase())];
    if status.state == "OPEN" {
        parts.push(format!("{} / {}", status.mergeable.to_lowercase(), status.merge_state_status.to_lowercase()));
        parts.push(if status.auto_merge { "auto-merge armed".to_string() } else { "auto-merge off".to_string() });
        if status.in_merge_queue {
            parts.push("in the merge queue".to_string());
        }
        parts.push(format!("{} check(s), {pending} pending", status.checks.len()));
        if !failed.is_empty() {
            parts.push(format!("failed: {}", failed.join(", ")));
        }
        let threads = status.new_unresolved_threads(armed_at);
        if threads > 0 {
            parts.push(format!("{threads} new unresolved thread(s)"));
        }
    }
    parts.join("; ")
}

/// Render `scheduler.review_prompt` for a relaunch.
pub fn review_prompt(template: &str, relaunch: &ReviewRelaunch, pr_url: &str) -> String {
    template
        .replace("{pr}", &relaunch.pr_number.to_string())
        .replace("{url}", pr_url)
        .replace("{reason}", &relaunch.reason)
        .replace("{detail}", &relaunch.detail)
        .trim()
        .to_string()
}

/// Apply one poll result to an `in_review` task. Mutates `task` (state,
/// review bookkeeping) and returns the effects for the daemon: events, a
/// Linear comment, branch deletion. Tasks in any other state are left alone.
pub fn on_pr_status(task: &mut Task, status: &PrStatus, cfg: &SchedulerConfig, now: DateTime<Utc>) -> Vec<Effect> {
    let mut effects = Vec::new();
    if task.state != TaskState::InReview {
        return effects;
    }
    let mut watch = task.review.clone().unwrap_or_else(|| ReviewWatch::armed(now, None));
    watch.last_polled_at = Some(now);
    let print = fingerprint(status, watch.armed_at);
    if watch.fingerprint.as_deref() != Some(print.as_str()) {
        watch.fingerprint = Some(print.clone());
        watch.last_change_at = now;
        effects.push(Effect::Log {
            level: EventLevel::Info,
            kind: "review.status".into(),
            message: describe(status, watch.armed_at),
            data: serde_json::json!({ "pr": task.pr_url, "status": status, "fingerprint": print }),
        });
    }

    let failed = status.failed_required_checks();
    let threads = status.new_unresolved_threads(watch.armed_at);
    let relaunch = if status.state == "MERGED" {
        task.state = TaskState::Completed;
        task.completed_at = Some(now);
        task.last_error = None;
        watch.waiting_manual_merge = false;
        effects.push(log(EventLevel::Info, "review.merged", format!("PR #{} was merged; task completed", status.number)));
        effects.push(Effect::DeleteBranch);
        None
    } else if status.state == "CLOSED" {
        park(task, CLOSED_UNMERGED.to_string());
        watch.waiting_manual_merge = false;
        effects.push(log(EventLevel::Warn, "review.closed", format!("PR #{} was closed without merging", status.number)));
        None
    } else if status.mergeable == "CONFLICTING" {
        Some(("conflict", String::new()))
    } else if !failed.is_empty() {
        Some(("ci_failed", failed.join(", ")))
    } else if let Some(why) = status.dropped_from_queue(watch.armed_at) {
        Some(("ci_failed", format!("merge queue: {why}")))
    } else if threads > 0 {
        Some(("review", format!("{threads} unresolved thread(s)")))
    } else {
        // The label alone: a held PR with green checks and no required
        // approvals is `CLEAN`, not `BLOCKED`.
        let hold = status.has_label(&cfg.merge_hold_label);
        if hold != watch.waiting_manual_merge {
            watch.waiting_manual_merge = hold;
            let message = if hold {
                format!("PR #{} is labelled `{}`: waiting for a manual merge", status.number, cfg.merge_hold_label)
            } else {
                format!("PR #{} is no longer held for a manual merge", status.number)
            };
            effects.push(log(EventLevel::Info, "review.hold", message));
        }
        let stale = cfg.review_stale_hours > 0 && now - watch.last_change_at >= Duration::hours(cfg.review_stale_hours as i64);
        if stale && !hold {
            let since = watch.last_change_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let reason =
                format!("PR #{} has not changed since {since} (review_stale_hours = {})", status.number, cfg.review_stale_hours);
            park(task, reason.clone());
            effects.push(log(EventLevel::Warn, "review.stale", reason));
            effects.push(Effect::LinearComment {
                body: format!(
                    "powerqueue: el PR {} no ha cambiado en {} h ({}). La tarea queda en `needs_attention`; \
                     `powerqueue task resume {}` la vuelve a vigilar.",
                    task.pr_url.as_deref().unwrap_or("?"),
                    cfg.review_stale_hours,
                    describe(status, watch.armed_at),
                    task.key
                ),
            });
        }
        None
    };

    if let Some((reason, detail)) = relaunch {
        let request = ReviewRelaunch { pr_number: status.number, reason: reason.to_string(), detail, requested_at: now };
        let what =
            format!("{reason}{}", if request.detail.is_empty() { String::new() } else { format!(" ({})", request.detail) });
        watch.waiting_manual_merge = false;
        if watch.rounds >= cfg.review_rounds_max {
            park(
                task,
                format!("PR #{} needs work ({what}) but review_rounds_max = {} is used up", status.number, cfg.review_rounds_max),
            );
            effects.push(log(
                EventLevel::Warn,
                "review.rounds_exhausted",
                format!(
                    "{what}: {} review round(s) already used; a human must decide (`task retry` runs one more round)",
                    watch.rounds
                ),
            ));
            effects.push(Effect::LinearComment {
                body: format!(
                    "powerqueue: el PR {} necesita trabajo ({what}), pero ya se usaron las {} rondas de revisión. \
                     La tarea queda en `needs_attention`.",
                    task.pr_url.as_deref().unwrap_or("?"),
                    cfg.review_rounds_max
                ),
            });
        } else {
            watch.rounds += 1;
            watch.attempt_base = task.attempts;
            task.state = TaskState::Queued;
            task.not_before = None;
            task.last_error = None;
            effects.push(Effect::Log {
                level: EventLevel::Info,
                kind: "review.relaunch".into(),
                message: format!(
                    "PR #{} needs work ({what}); resuming the session (review round {} of {})",
                    status.number, watch.rounds, cfg.review_rounds_max
                ),
                data: serde_json::json!({ "reason": request.reason, "detail": request.detail, "round": watch.rounds }),
            });
        }
        watch.relaunch = Some(request);
    }
    watch.parked = task.state == TaskState::NeedsAttention;
    task.review = Some(watch);
    effects
}

/// `powerqueue task retry` of a task in review: resume the session for a
/// review round now, whatever the PR says (the user wants the agent back on
/// it). User rounds do not count against `review_rounds_max`.
/// Returns the event to log, or `None` (task untouched) when the task is not
/// in review or its PR number cannot be read from `pr_url`.
pub fn request_round(task: &mut Task, now: DateTime<Utc>) -> Option<Effect> {
    if task.state != TaskState::InReview {
        return None;
    }
    let pr_number = task.pr_url.as_deref().and_then(pr_number_of)?;
    let mut watch = task.review.clone().unwrap_or_else(|| ReviewWatch::armed(now, None));
    watch.attempt_base = task.attempts;
    watch.waiting_manual_merge = false;
    watch.parked = false;
    watch.relaunch = Some(ReviewRelaunch { pr_number, reason: "requested".into(), detail: "by user".into(), requested_at: now });
    task.state = TaskState::Queued;
    task.not_before = None;
    task.last_error = None;
    task.review = Some(watch);
    Some(Effect::Log {
        level: EventLevel::Info,
        kind: "review.relaunch".into(),
        message: format!("PR #{pr_number}: review round requested by user; resuming the session"),
        data: serde_json::json!({ "reason": "requested", "detail": "by user" }),
    })
}

/// PR number of a GitHub PR URL, as `task complete --pr` accepts it
/// (`.../pull/1084`, `.../pull/1084/files`, ...); `None` when it is not one.
pub fn pr_number_of(url: &str) -> Option<u64> {
    url.parse::<crate::github::PrRef>().ok().map(|pr| pr.number)
}

/// Hand the task to a human: `needs_attention` with `reason`.
fn park(task: &mut Task, reason: String) {
    task.state = TaskState::NeedsAttention;
    task.last_error = Some(reason);
    task.not_before = None;
}

fn log(level: EventLevel, kind: &str, message: String) -> Effect {
    Effect::Log { level, kind: kind.to_string(), message, data: serde_json::json!({}) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskSource;
    use crate::github::{PrCheck, PrThread, QueueRemoval};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-06T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn status(state: &str, mergeable: &str) -> PrStatus {
        PrStatus {
            number: 7,
            state: state.into(),
            mergeable: mergeable.into(),
            merge_state_status: "CLEAN".into(),
            auto_merge: true,
            in_merge_queue: false,
            queue_removals: Vec::new(),
            labels: Vec::new(),
            head_oid: Some("0123456789".into()),
            checks: vec![PrCheck { name: "test".into(), conclusion: "SUCCESS".into(), required: true }],
            threads: Vec::new(),
        }
    }

    fn task() -> Task {
        let mut t = Task::new("PR-1", "Ship it", TaskSource::Manual);
        t.state = TaskState::InReview;
        t.attempts = 2;
        t.pr_url = Some("https://github.com/o/r/pull/7".into());
        t.review = Some(ReviewWatch::armed(now() - Duration::hours(1), None));
        t
    }

    fn kinds(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .map(|e| match e {
                Effect::Log { kind, .. } => kind.clone(),
                Effect::LinearComment { .. } => "linear_comment".into(),
                Effect::DeleteBranch => "delete_branch".into(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn merged_completes_and_deletes_the_branch_without_moving_linear() {
        let mut t = task();
        let fx = on_pr_status(&mut t, &status("MERGED", "UNKNOWN"), &SchedulerConfig::default(), now());
        assert_eq!(t.state, TaskState::Completed);
        assert_eq!(kinds(&fx), vec!["review.status", "review.merged", "delete_branch"]);
        assert!(!fx.iter().any(|e| matches!(e, Effect::Linear { .. })));
    }

    #[test]
    fn closed_without_merge_needs_attention() {
        let mut t = task();
        on_pr_status(&mut t, &status("CLOSED", "UNKNOWN"), &SchedulerConfig::default(), now());
        assert_eq!(t.state, TaskState::NeedsAttention);
        assert_eq!(t.last_error.as_deref(), Some(CLOSED_UNMERGED));
        assert!(t.parked_in_review(), "resume watches the PR again");
    }

    #[test]
    fn conflict_failed_checks_and_new_threads_relaunch_in_that_order() {
        let cfg = SchedulerConfig::default();
        let mut t = task();
        let mut st = status("OPEN", "CONFLICTING");
        st.checks[0].conclusion = "FAILURE".into();
        let fx = on_pr_status(&mut t, &st, &cfg, now());
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(kinds(&fx), vec!["review.status", "review.relaunch"]);
        let review = t.review.clone().unwrap();
        assert_eq!((review.rounds, review.attempt_base), (1, 2));
        let relaunch = review.relaunch.unwrap();
        assert_eq!((relaunch.reason.as_str(), relaunch.pr_number), ("conflict", 7));
        assert_eq!(
            review_prompt(&cfg.review_prompt, t.review_relaunch().unwrap(), "u"),
            "/ship-pr 7 --reason conflict",
            "no trailing space without a detail"
        );

        let mut t = task();
        st.mergeable = "MERGEABLE".into();
        st.checks.push(PrCheck { name: "lint".into(), conclusion: "FAILURE".into(), required: false });
        st.checks.push(PrCheck { name: "checks (typecheck)".into(), conclusion: "FAILURE".into(), required: true });
        on_pr_status(&mut t, &st, &cfg, now());
        let relaunch = t.review_relaunch().unwrap().clone();
        assert_eq!((relaunch.reason.as_str(), relaunch.detail.as_str()), ("ci_failed", "checks (typecheck), test"));
        assert_eq!(
            review_prompt(&cfg.review_prompt, &relaunch, "u"),
            "/ship-pr 7 --reason ci_failed checks (typecheck), test",
            "names with spaces stay apart"
        );

        let mut t = task();
        let armed = t.review.as_ref().unwrap().armed_at;
        let mut st = status("OPEN", "MERGEABLE");
        st.threads = vec![
            PrThread { resolved: false, last_comment_at: Some(armed - Duration::minutes(5)) },
            PrThread { resolved: false, last_comment_at: Some(armed + Duration::minutes(5)) },
        ];
        on_pr_status(&mut t, &st, &cfg, now());
        assert_eq!(
            t.review_relaunch().map(|r| (r.reason.as_str(), r.detail.as_str())),
            Some(("review", "1 unresolved thread(s)"))
        );
    }

    #[test]
    fn a_pr_dropped_by_the_merge_queue_relaunches_once() {
        let cfg = SchedulerConfig::default();
        let mut t = task();
        let armed = t.review.as_ref().unwrap().armed_at;
        let mut st = status("OPEN", "MERGEABLE");
        st.auto_merge = false;
        st.queue_removals = vec![QueueRemoval { at: armed + Duration::minutes(20), reason: "failed checks".into() }];
        on_pr_status(&mut t, &st, &cfg, now());
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(
            t.review_relaunch().map(|r| (r.reason.as_str(), r.detail.as_str())),
            Some(("ci_failed", "merge queue: failed checks"))
        );

        // A new hand-off after the removal: the old drop no longer counts.
        let mut t = task();
        t.review = Some(ReviewWatch::armed(armed + Duration::minutes(30), None));
        on_pr_status(&mut t, &st, &cfg, now());
        assert_eq!(t.state, TaskState::InReview);

        // Merged out of the queue, or a person dequeued it: no relaunch.
        for reason in ["merged", "manual"] {
            let mut t = task();
            st.queue_removals[0].reason = reason.into();
            on_pr_status(&mut t, &st, &cfg, now());
            assert_eq!(t.state, TaskState::InReview, "{reason}");
        }
    }

    #[test]
    fn user_can_request_a_round_while_in_review() {
        // AVS-1733: `task retry` of an in-review task used to do nothing.
        let mut t = task();
        t.attempts = 2;
        t.pr_url = Some("https://github.com/o/r/pull/1084".into());
        t.review.as_mut().unwrap().rounds = 5;
        t.review.as_mut().unwrap().waiting_manual_merge = true;
        let fx = request_round(&mut t, now()).expect("a round is started");
        assert_eq!(kinds(&[fx]), vec!["review.relaunch"]);
        assert_eq!(t.state, TaskState::Queued);
        let watch = t.review.as_ref().unwrap();
        assert_eq!((watch.rounds, watch.attempt_base, watch.waiting_manual_merge), (5, 2, false), "user rounds are not counted");
        let relaunch = t.review_relaunch().unwrap();
        assert_eq!((relaunch.pr_number, relaunch.reason.as_str()), (1084, "requested"));
        assert_eq!(pr_number_of("https://github.com/o/r/pull/12/files"), Some(12));

        let mut t = task();
        t.state = TaskState::Running;
        assert!(request_round(&mut t, now()).is_none());
        let mut t = task();
        t.pr_url = None;
        assert!(request_round(&mut t, now()).is_none());
        assert_eq!(t.state, TaskState::InReview);
    }

    #[test]
    fn rounds_are_capped() {
        let cfg = SchedulerConfig { review_rounds_max: 2, ..SchedulerConfig::default() };
        let mut t = task();
        t.review.as_mut().unwrap().rounds = 2;
        let fx = on_pr_status(&mut t, &status("OPEN", "CONFLICTING"), &cfg, now());
        assert_eq!(t.state, TaskState::NeedsAttention);
        assert_eq!(kinds(&fx), vec!["review.status", "review.rounds_exhausted", "linear_comment"]);
        assert!(t.review_relaunch().is_some(), "`task retry` runs the pending round");
        assert!(t.parked_in_review(), "`task resume` only watches the PR again");
        t.review.as_mut().unwrap().rearm(now());
        assert!(t.review_relaunch().is_none() && !t.review.as_ref().unwrap().parked);
    }

    #[test]
    fn unchanged_pr_waits_then_goes_stale_unless_held() {
        let cfg = SchedulerConfig::default();
        let mut t = task();
        let st = status("OPEN", "MERGEABLE");
        let fx = on_pr_status(&mut t, &st, &cfg, now());
        assert_eq!(t.state, TaskState::InReview);
        assert_eq!(kinds(&fx), vec!["review.status"]);
        // Same status later: no event, still waiting.
        assert!(on_pr_status(&mut t, &st, &cfg, now() + Duration::hours(2)).is_empty());
        let fx = on_pr_status(&mut t, &st, &cfg, now() + Duration::hours(24));
        assert_eq!(t.state, TaskState::NeedsAttention);
        assert_eq!(kinds(&fx), vec!["review.stale", "linear_comment"]);

        // Green checks and no required approvals: GitHub says `CLEAN`, and the
        // label alone holds it.
        let mut t = task();
        let mut held = st.clone();
        held.auto_merge = false;
        held.labels = vec!["Merge/Hold".into()];
        let fx = on_pr_status(&mut t, &held, &cfg, now());
        assert_eq!(kinds(&fx), vec!["review.status", "review.hold"]);
        assert!(t.review.as_ref().unwrap().waiting_manual_merge);
        assert!(on_pr_status(&mut t, &held, &cfg, now() + Duration::hours(48)).is_empty(), "a held PR is never stale");
        assert_eq!(t.state, TaskState::InReview);
    }

    #[test]
    fn other_states_are_ignored() {
        let mut t = task();
        t.state = TaskState::Running;
        assert!(on_pr_status(&mut t, &status("MERGED", "UNKNOWN"), &SchedulerConfig::default(), now()).is_empty());
        assert_eq!(t.state, TaskState::Running);
    }

    #[test]
    fn an_unusable_pr_parks_the_task_for_a_human() {
        let mut t = task();
        t.pr_url = Some("not a url".into());
        t.not_before = Some(now() + Duration::hours(1));
        let effects = on_unusable_pr(&mut t, "invalid pull request URL");
        assert_eq!(t.state, TaskState::NeedsAttention);
        assert_eq!(t.not_before, None, "parked like every other park: no stale retry time");
        assert_eq!(t.last_error.as_deref(), Some("in review without a usable pull request: invalid pull request URL"));
        assert!(matches!(&effects[..], [Effect::Log { kind, data, .. }] if kind == "review.error" && data["pr"] == "not a url"));
    }

    #[test]
    fn a_watch_error_is_noted_once_and_cleared_on_recovery() {
        let mut t = task();
        let effects = on_watch_error(&mut t, "o/r#7", "gh: command not found", now());
        assert_eq!(kinds(&effects), ["review.error"]);
        assert_eq!(t.state, TaskState::InReview, "the task is not changed");
        assert_eq!(t.last_error.as_deref(), Some("PR watcher: cannot read o/r#7: gh: command not found"));
        assert_eq!(t.review.as_ref().and_then(|w| w.last_polled_at), Some(now()), "retried at the next interval");
        assert!(
            on_watch_error(&mut t, "o/r#7", "gh: command not found", now() + Duration::minutes(1)).is_empty(),
            "same message: silent"
        );
        assert_eq!(kinds(&on_watch_error(&mut t, "o/r#7", "403", now())), ["review.error"], "a new message is logged");

        on_watch_recovered(&mut t);
        assert_eq!(t.last_error, None);
        t.last_error = Some(CLOSED_UNMERGED.into());
        on_watch_recovered(&mut t);
        assert_eq!(t.last_error.as_deref(), Some(CLOSED_UNMERGED), "only watcher errors are cleared");

        // A task without a watch gets one so the poll interval applies.
        let mut t = task();
        t.review = None;
        on_watch_error(&mut t, "o/r#7", "x", now());
        assert!(t.review.is_some());
    }
}
