//! `proptest` generators for the pure core (tests only).
//!
//! Every property in `scheduler::{commands,transitions,launch,review,
//! relay}` and `budget::{period,ledger,policy,estimator,probe}` draws its
//! inputs from here, so the shape of "a task the daemon could hold" is
//! written once. Values are correlated the way the daemon correlates them:
//! a task in `in_review` has a PR URL and a watch, a session is live iff its
//! task expects one, a crashed session carries an error.
//!
//! Instants are generated around a fixed [`ORIGIN`] (a Thursday noon) so
//! failures shrink to readable timestamps; keep generated durations small
//! (`chrono::Duration::seconds` panics far out of range).

#![allow(dead_code)]

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use proptest::collection::vec;
use proptest::prelude::*;

use crate::budget::{AnchorSource, Decision, Ledger, Ledgers, Period, Prediction, RateLimitState, TierLedger};
use crate::config::{BudgetConfig, SchedulerConfig};
use crate::domain::{
    Criticality, HookEvent, LinkedIssue, ModelTier, Provider, ReviewRelaunch, ReviewWatch, Session, SessionState, Task, TaskId,
    TaskSource, TaskState,
};
use crate::github::{PrCheck, PrStatus, PrThread, QueueRemoval};
use crate::session::{HookOutcome, SessionProbe};

/// `2026-10-01T12:00:00Z`: "now" for every property unless it says otherwise.
pub fn origin() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z").expect("valid origin").with_timezone(&Utc)
}

/// An instant between `origin - before` and `origin + after` (seconds, whole).
pub fn instant_between(before_secs: i64, after_secs: i64) -> impl Strategy<Value = DateTime<Utc>> {
    (-before_secs..=after_secs).prop_map(|s| origin() + Duration::seconds(s))
}

/// An instant within three days either side of the origin.
pub fn instant() -> impl Strategy<Value = DateTime<Utc>> {
    instant_between(3 * 86_400, 3 * 86_400)
}

/// An instant up to three days before the origin (a past event).
pub fn past_instant() -> impl Strategy<Value = DateTime<Utc>> {
    instant_between(3 * 86_400, 0)
}

/// An instant strictly after the origin, up to three days ahead.
pub fn future_instant() -> impl Strategy<Value = DateTime<Utc>> {
    instant_between(-1, 3 * 86_400)
}

pub fn task_state() -> impl Strategy<Value = TaskState> {
    prop::sample::select(TaskState::ALL.to_vec())
}

pub fn session_state() -> impl Strategy<Value = SessionState> {
    prop::sample::select(vec![
        SessionState::Launching,
        SessionState::Running,
        SessionState::Idle,
        SessionState::Exited,
        SessionState::Crashed,
        SessionState::Killed,
    ])
}

pub fn criticality() -> impl Strategy<Value = Criticality> {
    prop::sample::select(Criticality::ALL.to_vec())
}

pub fn provider() -> impl Strategy<Value = Provider> {
    prop::sample::select(Provider::ALL.to_vec())
}

/// The shipped models of every provider (what `BudgetConfig::default` knows).
pub fn known_models() -> Vec<ModelTier> {
    let cfg = BudgetConfig::default();
    Provider::ALL.iter().flat_map(|p| cfg.models_for(*p)).collect()
}

/// One of the shipped Claude models.
pub fn claude_model() -> impl Strategy<Value = ModelTier> {
    prop::sample::select(BudgetConfig::default().models_for(Provider::Claude))
}

/// One of the shipped models of any provider.
pub fn model() -> impl Strategy<Value = ModelTier> {
    prop::sample::select(known_models())
}

/// Short identifier-like text (keys, names, reasons).
pub fn word() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9-]{0,7}"
}

/// A sentence-ish line of text, possibly empty.
pub fn line() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 ,.?!'-]{0,40}"
}

/// A GitHub PR URL `request_round` / the watcher can parse.
pub fn pr_url() -> impl Strategy<Value = String> {
    (1u64..5000).prop_map(|n| format!("https://github.com/o/r/pull/{n}"))
}

/// A PR URL, usable or not.
pub fn any_pr_url() -> impl Strategy<Value = String> {
    prop_oneof![4 => pr_url(), 1 => Just("not a pr".to_string()), 1 => Just("https://gitlab.com/o/r/-/merge_requests/3".to_string())]
}

pub fn linked_issue() -> impl Strategy<Value = LinkedIssue> {
    (word(), prop::sample::select(vec!["unstarted", "started", "completed", "canceled", "cancelled", "backlog"]), any::<bool>())
        .prop_map(|(key, state_type, pr_merged)| LinkedIssue {
            key: format!("ENG-{key}"),
            title: String::new(),
            state_type: state_type.to_string(),
            pr_merged,
        })
}

pub fn review_relaunch() -> impl Strategy<Value = ReviewRelaunch> {
    (1u64..5000, prop::sample::select(vec!["conflict", "ci_failed", "review", "requested"]), line(), past_instant()).prop_map(
        |(pr_number, reason, detail, requested_at)| ReviewRelaunch {
            pr_number,
            reason: reason.to_string(),
            detail,
            requested_at,
        },
    )
}

/// A PR watch armed some time before the origin.
pub fn review_watch() -> impl Strategy<Value = ReviewWatch> {
    (
        instant_between(3 * 86_400, 0),
        0u32..=6,
        prop::option::of(past_instant()),
        prop::option::of(line()),
        any::<bool>(),
        0u32..=5,
        prop::option::of(Just("/wt/eng-1".to_string())),
        prop::option::of(review_relaunch()),
        any::<bool>(),
    )
        .prop_map(
            |(
                armed_at,
                rounds,
                last_polled_at,
                fingerprint,
                waiting_manual_merge,
                attempt_base,
                worktree_path,
                relaunch,
                parked,
            )| {
                ReviewWatch {
                    armed_at,
                    rounds,
                    last_polled_at,
                    // Never after the arming, never after the origin.
                    last_change_at: armed_at,
                    fingerprint,
                    waiting_manual_merge,
                    attempt_base,
                    worktree_path,
                    relaunch,
                    parked,
                }
            },
        )
}

/// What a task row looks like in each state. `state` fixes the state; the
/// rest is drawn so the row is one the daemon could hold: `in_review` has a
/// parseable PR URL and a watch, terminal tasks have `completed_at`, a task
/// that ran has a branch.
pub fn task_in(state: TaskState) -> impl Strategy<Value = Task> {
    let ran = !matches!(state, TaskState::Queued | TaskState::Blocked | TaskState::Paused | TaskState::Throttled);
    let in_review = state == TaskState::InReview;
    let terminal = state.is_terminal();
    let identity = (any::<u128>(), word(), criticality(), 0.0f64..2000.0, 0u32..=5, prop::option::of(1u32..=5), past_instant());
    let bookkeeping = (
        prop::option::of(future_instant()),
        prop::option::of(line()),
        prop::option::of(line()),
        prop::option::of(claude_model()),
        if in_review { pr_url().prop_map(Some).boxed() } else { prop::option::weighted(0.2, pr_url()).boxed() },
        if in_review { review_watch().prop_map(Some).boxed() } else { prop::option::weighted(0.3, review_watch()).boxed() },
    );
    let links = (
        vec(linked_issue(), 0..3),
        vec(linked_issue(), 0..3),
        vec(word(), 0..3),
        prop::option::of(prop::sample::select(vec![1.0, 2.0, 3.0, 5.0, 8.0])),
    );
    (identity, bookkeeping, links).prop_map(
        move |(
            (id, key, criticality, score, attempts, max_attempts, created_at),
            (not_before, last_error, summary, model_override, pr_url, review),
            (blocked_by, children, labels, estimate),
        )| {
            let mut t = Task::new(format!("ENG-{key}"), "t", TaskSource::Manual);
            t.id = TaskId(uuid::Uuid::from_u128(id));
            t.state = state;
            t.criticality = criticality;
            t.score = score;
            t.attempts = if ran { attempts.max(1) } else { attempts };
            t.max_attempts = max_attempts;
            t.not_before = not_before;
            t.last_error = last_error;
            t.summary = summary;
            t.model_override = model_override;
            t.pr_url = pr_url;
            t.review = review;
            t.blocked_by = blocked_by;
            t.children = children;
            t.labels = labels;
            t.estimate = estimate;
            t.created_at = created_at;
            t.updated_at = created_at;
            if ran {
                t.branch = Some(format!("pq/{}", t.slug()));
                t.worktree_path = Some(format!("/wt/{}", t.slug()));
                t.started_at = Some(created_at);
                t.model = Some(ModelTier::sonnet());
            }
            if terminal {
                t.completed_at = Some(origin() - Duration::minutes(1));
            }
            t
        },
    )
}

/// A task in any state (see [`task_in`]).
pub fn task() -> impl Strategy<Value = Task> {
    task_state().prop_flat_map(task_in)
}

/// A task in one of `states`.
pub fn task_among(states: &[TaskState]) -> impl Strategy<Value = Task> {
    prop::sample::select(states.to_vec()).prop_flat_map(task_in)
}

/// A session row of `task`: same id and attempt, live iff the task expects
/// one ([`TaskState::has_live_session`]), started before the origin.
pub fn session_for(task: &Task) -> BoxedStrategy<Session> {
    let task_id = task.id;
    let attempt = task.attempts.max(1);
    let live = task.state.has_live_session();
    let state = if live {
        prop::sample::select(vec![SessionState::Launching, SessionState::Running, SessionState::Idle]).boxed()
    } else {
        prop::sample::select(vec![SessionState::Exited, SessionState::Crashed, SessionState::Killed]).boxed()
    };
    (
        any::<u128>(),
        state,
        model(),
        instant_between(6 * 3600, 0),
        0i64..=3600,
        prop::option::weighted(0.3, instant_between(3600, 0)),
        0i64..=3600,
        prop::option::of(Just("thread-1".to_string())),
        prop::option::of(Just("/transcripts/s.jsonl".to_string())),
    )
        .prop_map(
            move |(id, state, model, started_at, activity_gap, waiting_since, waited_secs, agent_session_id, transcript_path)| {
                let last_activity_at = (started_at + Duration::seconds(activity_gap)).min(origin());
                let ended = !state.is_live();
                Session {
                    id: uuid::Uuid::from_u128(id),
                    task_id,
                    attempt,
                    model,
                    state,
                    tmux_session: "powerqueue".into(),
                    tmux_window: "@1".into(),
                    pane_id: Some("%1".into()),
                    pid: Some(42),
                    transcript_path,
                    exit_code: if ended { Some(1) } else { None },
                    started_at,
                    ended_at: if ended { Some(last_activity_at) } else { None },
                    last_activity_at,
                    error: if state == SessionState::Crashed { Some("boom".into()) } else { None },
                    agent_session_id,
                    waiting_since: waiting_since.filter(|_| !ended).map(|w| w.max(started_at)),
                    waited_secs,
                }
            },
        )
        .boxed()
}

/// A task with a matching session (see [`session_for`]).
pub fn task_with_session() -> impl Strategy<Value = (Task, Session)> {
    task().prop_flat_map(|t| {
        let s = session_for(&t);
        (Just(t), s)
    })
}

/// A task in one of `states` with a matching session.
pub fn task_with_session_among(states: &[TaskState]) -> impl Strategy<Value = (Task, Session)> {
    task_among(states).prop_flat_map(|t| {
        let s = session_for(&t);
        (Just(t), s)
    })
}

/// Scheduler knobs in a small range (backoff lists of 0..=3 entries so the
/// "last value repeats" rule is exercised).
pub fn scheduler_config() -> impl Strategy<Value = SchedulerConfig> {
    (1u32..=5, vec(1u64..=600, 0..=3), 0u64..=600, 0u64..=3600, 0u64..=7200, 0u32..=5, 0u64..=48).prop_map(
        |(
            max_attempts,
            restart_backoff_secs,
            idle_timeout_secs,
            stale_session_secs,
            max_session_secs,
            review_rounds_max,
            review_stale_hours,
        )| {
            SchedulerConfig {
                max_attempts,
                restart_backoff_secs,
                idle_timeout_secs,
                stale_session_secs,
                max_session_secs,
                review_rounds_max,
                review_stale_hours,
                ..SchedulerConfig::default()
            }
        },
    )
}

/// Floats are whole or half numbers so a prediction survives a JSON round
/// trip bit for bit (`serde_json` parses floats inexactly unless its
/// `float_roundtrip` feature is on; a replayed trace must compare equal).
pub fn prediction() -> impl Strategy<Value = Prediction> {
    (1u64..5_000_000, 1u64..20_000, 0u8..=10).prop_map(|(weighted_tokens, wall_secs, confidence)| Prediction {
        weighted_tokens: weighted_tokens as f64,
        wall_secs: wall_secs as f64,
        confidence: confidence as f64 / 10.0,
        basis: "prop".into(),
    })
}

/// A policy decision: a model, or a throttle with an optional retry time.
pub fn decision() -> impl Strategy<Value = Decision> {
    (prop::option::of(model()), prop::option::of(future_instant()), prediction()).prop_map(|(model, retry_at, prediction)| {
        Decision { retry_at: if model.is_some() { None } else { retry_at }, model, prediction, reasons: vec!["prop".into()] }
    })
}

/// A ledger of `provider` at the origin: the shipped models as tiers, a
/// period of 1..=14 days containing the origin at any elapsed fraction, a
/// five-hour window, budgets up to 1e9 and each tier spent 0..=120% of its
/// share. The observation is left `None` (set it in the property if needed).
pub fn ledger_for(provider: Provider) -> impl Strategy<Value = Ledger> {
    let cfg = BudgetConfig::default();
    let models = cfg.models_for(provider);
    let n = models.len();
    (1i64..=14, 0.0f64..1.0, 1.0f64..1.0e9, 0.0f64..1.0e9, vec(0.0f64..1.2, n), any::<bool>()).prop_map(
        move |(days, elapsed, period_budget, window_budget, spent, window_enabled)| {
            let len = Duration::days(days);
            let start = origin() - Duration::seconds((len.num_seconds() as f64 * elapsed) as i64);
            let period = Period { start, end: start + len };
            let mut ledger =
                Ledger::blank(provider, origin(), period, Period { start: origin() - Duration::hours(5), end: origin() });
            let cfg = BudgetConfig::default();
            ledger.tiers = models
                .iter()
                .zip(spent.iter())
                .map(|(tier, frac)| {
                    let budget = period_budget * crate::budget::tier_share(&cfg, tier);
                    TierLedger {
                        tier: tier.clone(),
                        period_weighted: budget * frac,
                        window_weighted: budget * frac * 0.1,
                        period_budget: budget,
                        ..Default::default()
                    }
                })
                .collect();
            ledger.total_period_weighted = ledger.tiers.iter().map(|t| t.period_weighted).sum();
            ledger.total_window_weighted = ledger.tiers.iter().map(|t| t.window_weighted).sum();
            ledger.period_budget = period_budget;
            ledger.window_budget = window_budget;
            ledger.configured_period_budget = period_budget;
            ledger.configured_window_budget = window_budget;
            ledger.window_enabled = window_enabled;
            ledger.anchor_source = AnchorSource::Config;
            ledger
        },
    )
}

/// Ledgers for a non-empty subset of providers (Claude always included so
/// the default config's enabled provider has one).
pub fn ledgers() -> impl Strategy<Value = Ledgers> {
    (ledger_for(Provider::Claude), prop::option::of(ledger_for(Provider::Codex)), prop::option::of(ledger_for(Provider::Gemini)))
        .prop_map(|(claude, codex, gemini)| {
            let mut by_provider = BTreeMap::new();
            by_provider.insert(Provider::Claude, claude);
            if let Some(l) = codex {
                by_provider.insert(Provider::Codex, l);
            }
            if let Some(l) = gemini {
                by_provider.insert(Provider::Gemini, l);
            }
            Ledgers { by_provider }
        })
}

/// A budget config with every provider enabled iff `enabled` says so
/// (Claude always on), the shipped models.
pub fn budget_config() -> impl Strategy<Value = BudgetConfig> {
    (any::<bool>(), any::<bool>(), 0.0f64..0.3, 0.5f64..1.0).prop_map(|(codex, gemini, safety_margin, endgame_fraction)| {
        let mut cfg = BudgetConfig::default();
        cfg.providers.codex.enabled = codex;
        cfg.providers.gemini.enabled = gemini;
        cfg.safety_margin = safety_margin;
        cfg.endgame_fraction = endgame_fraction;
        cfg
    })
}

/// Rate-limit marks on a few models, expiring around the origin.
pub fn rate_limits() -> impl Strategy<Value = RateLimitState> {
    vec((model(), instant_between(3600, 3600)), 0..4).prop_map(|marks| {
        let mut state = RateLimitState::default();
        for (tier, until) in marks {
            state.mark(tier, until);
        }
        state
    })
}

pub fn pr_check() -> impl Strategy<Value = PrCheck> {
    (
        prop::sample::select(vec!["test", "lint", "build"]),
        prop::sample::select(vec!["", "SUCCESS", "FAILURE", "NEUTRAL", "TIMED_OUT"]),
        any::<bool>(),
    )
        .prop_map(|(name, conclusion, required)| PrCheck { name: name.into(), conclusion: conclusion.into(), required })
}

pub fn pr_thread() -> impl Strategy<Value = PrThread> {
    (any::<bool>(), prop::option::of(instant_between(3 * 86_400, 0)))
        .prop_map(|(resolved, last_comment_at)| PrThread { resolved, last_comment_at })
}

pub fn queue_removal() -> impl Strategy<Value = QueueRemoval> {
    (instant_between(3 * 86_400, 0), prop::sample::select(vec!["merged", "manual", "failed checks", "conflict"]))
        .prop_map(|(at, reason)| QueueRemoval { at, reason: reason.into() })
}

/// What `gh` might report for PR `number`.
pub fn pr_status(number: u64) -> impl Strategy<Value = PrStatus> {
    (
        prop::sample::select(vec!["OPEN", "MERGED", "CLOSED"]),
        prop::sample::select(vec!["MERGEABLE", "CONFLICTING", "UNKNOWN"]),
        prop::sample::select(vec!["CLEAN", "BLOCKED", "BEHIND", "DIRTY", "UNKNOWN"]),
        any::<bool>(),
        any::<bool>(),
        vec(queue_removal(), 0..2),
        prop::sample::subsequence(vec!["merge/hold".to_string(), "bug".to_string(), "Merge/Hold".to_string()], 0..=2),
        prop::option::of("[0-9a-f]{10}"),
        vec(pr_check(), 0..3),
        vec(pr_thread(), 0..3),
    )
        .prop_map(
            move |(
                state,
                mergeable,
                merge_state_status,
                auto_merge,
                in_merge_queue,
                queue_removals,
                labels,
                head_oid,
                checks,
                threads,
            )| {
                PrStatus {
                    number,
                    state: state.into(),
                    mergeable: mergeable.into(),
                    merge_state_status: merge_state_status.into(),
                    auto_merge,
                    in_merge_queue,
                    queue_removals,
                    labels,
                    head_oid,
                    checks,
                    threads,
                }
            },
        )
}

pub fn hook_event() -> impl Strategy<Value = HookEvent> {
    prop::sample::select(HookEvent::ALL.to_vec())
}

/// Every hook outcome the daemon can drain, with small payloads.
pub fn hook_outcome() -> impl Strategy<Value = HookOutcome> {
    prop_oneof![
        (prop::option::of(Just("/t.jsonl".to_string())), prop::sample::select(vec!["startup", "resume", "compact", "clear"]))
            .prop_map(|(transcript_path, source)| HookOutcome::Started { transcript_path, source: source.into() }),
        line().prop_map(|summary| HookOutcome::Completed { summary }),
        (line(), line()).prop_map(|(reason, message)| HookOutcome::Blocked { reason, message }),
        line().prop_map(|last_message| HookOutcome::TurnEnded { last_message }),
        (prop::sample::select(vec!["rate_limit", "overloaded", "usage_limit", "quota"]), line())
            .prop_map(|(error_type, message)| HookOutcome::RateLimited { error_type: error_type.into(), message }),
        (prop::sample::select(vec!["authentication_failed", "api_error", "other"]), line())
            .prop_map(|(error_type, message)| HookOutcome::TurnFailed { error_type: error_type.into(), message }),
        prop::sample::select(vec!["clear", "logout", "prompt_input_exit", "other"])
            .prop_map(|reason| HookOutcome::SessionEnded { reason: reason.into() }),
        (prop::sample::select(vec!["permission_prompt", "idle_prompt", "other"]), line())
            .prop_map(|(kind, message)| HookOutcome::Notification { kind: kind.into(), message }),
        hook_event().prop_map(|event| HookOutcome::Activity { event }),
    ]
}

/// A liveness probe result: alive, dead with or without a status, or gone.
pub fn session_probe() -> impl Strategy<Value = SessionProbe> {
    prop_oneof![
        Just(SessionProbe {
            pane_exists: true,
            pane_dead: false,
            exit_status: None,
            pane_pid: Some(42),
            current_command: Some("node".into())
        }),
        prop::option::of(0i32..=130).prop_map(|exit_status| SessionProbe {
            pane_exists: true,
            pane_dead: true,
            exit_status,
            pane_pid: None,
            current_command: None
        }),
        Just(SessionProbe { pane_exists: false, pane_dead: true, exit_status: None, pane_pid: None, current_command: None }),
    ]
}

/// Pane text that may or may not show the "waiting for reset" signature.
pub fn pane_tail() -> impl Strategy<Value = Option<String>> {
    prop::option::of(prop_oneof![
        line(),
        Just("Usage limit reached · continuing automatically at 3:45pm · esc to cancel".to_string()),
        line().prop_map(|l| format!("{l}\ncontinuing automatically at 9am\n")),
    ])
}

/// The event kinds of a list of effects (`Log` kinds; other variants by name).
pub fn effect_kinds(effects: &[crate::scheduler::Effect]) -> Vec<String> {
    use crate::scheduler::Effect;
    effects
        .iter()
        .map(|e| match e {
            Effect::Log { kind, .. } => kind.clone(),
            Effect::Cleanup { .. } => "cleanup".into(),
            Effect::Linear { .. } => "linear".into(),
            Effect::Nudge { .. } => "nudge".into(),
            Effect::KillWindow => "kill_window".into(),
            Effect::RateLimit { .. } => "rate_limit".into(),
            Effect::RateLimitProvider { .. } => "rate_limit_provider".into(),
            Effect::ReleaseForReview => "release_for_review".into(),
            Effect::LinearComment { .. } => "linear_comment".into(),
            Effect::DeleteBranch => "delete_branch".into(),
            Effect::Question { .. } => "question".into(),
            Effect::ProgressComment { .. } => "progress_comment".into(),
            Effect::ForgetAnswer => "forget_answer".into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest! {
        #[test]
        fn generated_rows_are_consistent((task, session) in task_with_session()) {
            prop_assert_eq!(session.task_id, task.id);
            prop_assert_eq!(session.state.is_live(), task.state.has_live_session());
            prop_assert!(session.started_at <= origin());
            prop_assert!(session.last_activity_at <= origin());
            prop_assert!(session.last_activity_at >= session.started_at);
            if task.state == TaskState::InReview {
                prop_assert!(task.review.is_some());
                prop_assert!(task.pr_url.as_deref().and_then(crate::scheduler::review::pr_number_of).is_some());
            }
            if task.state.is_terminal() {
                prop_assert!(task.completed_at.is_some());
            }
            if let Some(w) = &task.review {
                prop_assert!(w.last_change_at <= origin());
            }
        }

        #[test]
        fn generated_ledgers_contain_the_origin(ledgers in ledgers()) {
            for (_, l) in ledgers.iter() {
                prop_assert!(l.period.contains(origin()), "{:?}", l.period);
                prop_assert_eq!(l.now, origin());
                prop_assert!(!l.tiers.is_empty());
            }
        }
    }
}
