use super::*;
use chrono::Duration;
use powerqueue::domain::EventLevel;
use serde_json::{Value, json};
use uuid::Uuid;

fn event(index: i64, kind: &str) -> Event {
    Event {
        id: index,
        task_id: Some(TaskId(Uuid::from_u128(1))),
        session_id: None,
        timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().with_timezone(&Utc) + Duration::seconds(index),
        level: EventLevel::Info,
        kind: kind.into(),
        data: json!({}),
        message: String::new(),
    }
}

fn payload(index: i64, kind: &str, data: Value) -> Event {
    Event { data, ..event(index, kind) }
}

fn launched() -> Vec<Event> {
    vec![
        event(1, "task.created"),
        payload(2, "task.starting", json!({"attempt": 1})),
        payload(3, "session.launched", json!({"attempt": 1})),
    ]
}

fn with_tail(tail: Vec<Event>) -> Vec<Event> {
    let mut rows = launched();
    rows.extend(tail);
    rows
}

fn crash(maximum: i64) -> Event {
    Event {
        message: format!("dead; retrying in 10s (attempt 1 of {maximum})"),
        ..payload(4, "session.crashed", json!({"attempt": 1, "max_attempts": maximum}))
    }
}

fn rejects(rows: Vec<Event>, text: &str) {
    let error = project(rows).unwrap_err().to_string();
    assert!(error.contains(text), "expected {text:?}, got {error:?}");
}

#[test]
fn nanos_and_newest_first_input() {
    let mut rows = launched();
    rows[0].timestamp = "2026-01-01T00:00:01.000000001Z".parse().unwrap();
    rows.reverse();
    let (observations, _) = project(rows).unwrap();
    assert_eq!(observations.iter().map(|o| o.op).collect::<Vec<_>>(), [Op::Create, Op::Start, Op::Launch]);
    assert_eq!(observations.iter().map(|o| o.time).collect::<Vec<_>>(), [0, 1, 2]);
}

#[test]
fn refuses_empty_vacuous_duplicate_and_unknown_events() {
    rejects(vec![], "nonempty JSON array");
    rejects(vec![event(1, "task.created")], "no in-scope launches");
    rejects(with_tail(vec![event(3, "task.completed")]), "duplicate event IDs");
    for kind in ["task.teleported", "session.teleported", "budget.unknown", "unknown"] {
        rejects(with_tail(vec![event(4, kind)]), "unmapped kind");
    }
}

#[test]
fn every_cutoff_ends_prefix_permanently() {
    for kind in OUT_OF_SCOPE.iter().copied().chain(["review.requeued", "relay.answer", "inbox.complete"]) {
        let rows =
            with_tail(vec![event(4, kind), event(5, "task.teleported"), payload(6, "task.starting", json!({"attempt": 2}))]);
        let (observations, report) = project(rows).unwrap();
        assert_eq!(observations.len(), 3, "{kind}");
        assert_eq!(report.truncated_or_skipped_tasks, BTreeMap::from([(format!("prefix ends at {kind}"), 1)]));
    }
}

#[test]
fn partial_history_is_reported_without_hiding_other_tasks() {
    let mut partial = event(0, "session.launched");
    partial.task_id = Some(TaskId(Uuid::from_u128(2)));
    let (observations, report) = project(with_tail(vec![partial])).unwrap();
    assert_eq!(observations.len(), 3);
    assert_eq!(report.tasks, 1);
    assert_eq!(report.truncated_or_skipped_tasks["missing initial creation (partial history)"], 1);
    let mut rows = launched();
    rows.remove(0);
    rejects(rows, "no in-scope launches");
}

#[test]
fn linear_creation_echo_is_consumed_only_once() {
    let mut rows = with_tail(vec![event(4, "task.created"), event(5, "task.created")]);
    rows[0].data = json!({"identifier": "EXAMPLE-1"});
    let mut global = event(0, "daemon.started");
    global.task_id = None;
    rows.push(global);
    let (observations, report) = project(rows).unwrap();
    assert_eq!(observations.iter().map(|o| o.op).collect::<Vec<_>>(), [Op::Create, Op::Start, Op::Launch, Op::Create]);
    assert_eq!(report.stutters["task.created (daemon echo of Linear sync)"], 1);
    assert_eq!(observations[0].time, 1); // global event establishes origin
}

#[test]
fn linear_cancellation_echo_is_consumed_only_once() {
    let cancel = Event { message: "issue closed in Linear".into(), ..event(5, "task.cancelled") };
    let second = Event { id: 6, timestamp: event(6, "").timestamp, ..cancel.clone() };
    let (observations, report) =
        project(with_tail(vec![payload(4, "task.cancelled", json!({"linear_state_type": "canceled"})), cancel, second])).unwrap();
    assert_eq!(observations[3].op, Op::SourceCancel);
    assert_eq!(observations[4].op, Op::Cancel);
    assert_eq!(report.stutters["task.cancelled (daemon echo of Linear sync)"], 1);
}

#[test]
fn ordinary_duplicate_creation_is_retained_for_quint_to_reject() {
    let (observations, _) = project(with_tail(vec![event(4, "task.created")])).unwrap();
    assert_eq!(observations[3].op, Op::Create);
}

#[test]
fn observational_events_stutter_but_permission_prompt_cuts_off() {
    let mut rows = launched();
    let kinds: Vec<_> = STUTTERS
        .iter()
        .filter(|kind| !OUT_OF_SCOPE.contains(kind))
        .map(|kind| (*kind).to_owned())
        .chain(STUTTER_PREFIXES.iter().map(|p| format!("{p}observed")))
        .collect();
    for (i, kind) in kinds.iter().enumerate() {
        rows.push(event(i as i64 + 4, kind));
    }
    let (observations, report) = project(rows).unwrap();
    assert_eq!(observations.len(), 3);
    assert_eq!(report.stutters, kinds.into_iter().map(|kind| (kind, 1)).collect());
}

#[test]
fn cooldown_deadlines_and_wake_are_projected() {
    let until = event(20, "").timestamp.to_rfc3339();
    let rows = with_tail(vec![
        payload(4, "budget.rate_limited", json!({"source": "pane"})),
        payload(5, "budget.rate_limited", json!({"until": until})),
        payload(6, "session.waiting_for_reset", json!({"until": until})),
        payload(7, "task.throttled", json!({"retry_at": until})),
        Event { message: "rate-limit cooldown passed; session still alive".into(), ..event(20, "task.resumed") },
    ]);
    let (observations, report) = project(rows).unwrap();
    assert_eq!(
        observations[3..].iter().map(|o| o.op).collect::<Vec<_>>(),
        [Op::RateLimit, Op::RateLimit, Op::Throttle, Op::Wake]
    );
    assert_eq!(observations[3..6].iter().map(|o| o.until).collect::<Vec<_>>(), [19, 19, 19]);
    assert_eq!(report.stutters["budget.rate_limited (provider only)"], 1);
}

#[test]
fn rejects_invalid_numeric_payloads_and_deadlines() {
    for kind in ["task.starting", "session.launched", "session.crashed", "task.failed"] {
        for bad in [Value::Null, json!(true), json!(0), json!(-1), json!(1.5), json!("1"), json!(u64::MAX)] {
            rejects(with_tail(vec![payload(4, kind, json!({"attempt": bad}))]), "invalid attempt");
        }
    }
    for kind in ["session.crashed", "task.failed"] {
        for bad in [Value::Null, json!(false), json!(0), json!(-1), json!(1.5), json!("1")] {
            rejects(with_tail(vec![payload(4, kind, json!({"attempt": 1, "max_attempts": bad}))]), "invalid max_attempts");
        }
    }
    for (kind, key) in [("task.throttled", "retry_at"), ("budget.rate_limited", "until"), ("session.waiting_for_reset", "until")]
    {
        rejects(with_tail(vec![event(4, kind)]), "timestamp");
        for bad in ["not a date", "2026-01-01T00:00:20"] {
            rejects(with_tail(vec![payload(4, kind, json!({key: bad}))]), "invalid");
        }
    }
}

#[test]
fn crash_backoff_is_required_and_overflow_checked() {
    let (observations, _) = project(with_tail(vec![crash(3)])).unwrap();
    assert_eq!((observations[3].until, observations[3].maximum), (13, 3));
    let mut row = crash(3);
    row.message.clear();
    rejects(with_tail(vec![row.clone()]), "missing crash backoff");
    row.message = format!("dead; retrying in {}s (attempt 1 of 3)", i64::MAX);
    rejects(with_tail(vec![row.clone()]), "crash backoff overflow");
    row.message = format!("dead; retrying in {}s (attempt 1 of 3)", u64::MAX);
    rejects(with_tail(vec![row]), "invalid crash backoff");
}

#[test]
fn crash_uses_recorded_deadline_instead_of_log_time() {
    let mut row = crash(3);
    // The transition ran at second 3; its event was persisted at second 4.
    row.data["retry_at"] = json!(event(13, "").timestamp);
    let (observations, _) = project(with_tail(vec![row.clone()])).unwrap();
    assert_eq!(observations[3].until, 12, "deadline is relative to creation at second 1");
    for invalid in [Value::Null, json!("not a date"), json!("2026-01-01T00:00:13")] {
        row.data["retry_at"] = invalid;
        rejects(with_tail(vec![row.clone()]), "timestamp");
    }
}

#[test]
fn clock_regression_including_stutters_is_rejected() {
    for kind in ["task.completed", "session.started"] {
        let row = Event { timestamp: event(1, "").timestamp, ..event(4, kind) };
        rejects(with_tail(vec![row]), "clock moved backwards");
    }
}

#[test]
fn task_indices_follow_first_event_and_output_is_anonymized() {
    let mut rows = launched();
    rows[0].message = "private ticket title and /private/path".into();
    rows[0].data = json!({"secret": "do not export"});
    for mut row in launched() {
        row.id += 3;
        row.timestamp += Duration::seconds(3);
        row.task_id = Some(TaskId(Uuid::from_u128(2)));
        rows.push(row);
    }
    rows.reverse();
    let (observations, report) = project(rows).unwrap();
    assert_eq!(observations.iter().map(|o| o.id).collect::<Vec<_>>(), [0, 0, 0, 1, 1, 1]);
    assert_eq!(report.tasks, 2);
    assert_eq!(report.actions, BTreeMap::from([("Create".into(), 2), ("Start".into(), 2), ("Launch".into(), 2)]));
    let artifacts = render(&observations) + &serde_json::to_string(&report).unwrap();
    for private in ["private ticket", "/private/path", "do not export", "00000000-0000-0000-0000-000000000001"] {
        assert!(!artifacts.contains(private));
    }
}

#[test]
fn parses_the_application_event_envelope_and_requires_timezone() {
    let serialized = serde_json::to_vec(&launched()).unwrap();
    let parsed: Vec<Event> = serde_json::from_slice(&serialized).unwrap();
    assert_eq!(project(parsed).unwrap().0.len(), 3);
    let mut value = serde_json::to_value(launched()).unwrap();
    value[0]["timestamp"] = json!("2026-01-01T00:00:01");
    assert!(serde_json::from_value::<Vec<Event>>(value).is_err());
}

#[test]
fn output_names_cannot_escape_generated_directory() {
    for bad in ["", "../escape", "/absolute", "a/b", "a.b", "a b", "é", "a\n"] {
        assert!(output_name(bad).is_err(), "{bad:?}");
    }
    for good in ["replay", "crash-resume", "test_01"] {
        assert_eq!(output_name(good).unwrap(), good);
    }
}

#[test]
fn rejected_observation_uses_last_valid_itf_index() {
    let (observations, _) = project(launched()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let trace = directory.path().join("counterexample.itf.json");
    fs::write(&trace, json!({"states": [{"index": {"#bigint": "1"}}, {}]}).to_string()).unwrap();
    let rejected: Value = serde_json::from_str(&rejected_observation(&trace, &observations).unwrap()).unwrap();
    assert_eq!(rejected["event"], 2);
    fs::write(&trace, json!({"states": [{"index": {"#bigint": "3"}}]}).to_string()).unwrap();
    assert!(rejected_observation(&trace, &observations).is_none());
    fs::write(&trace, "invalid").unwrap();
    assert!(rejected_observation(&trace, &observations).is_none());
}

// These tests intentionally fail if Quint is missing when explicitly requested.
// The ordinary Rust gate exercises projection without installing Node/Quint.
fn check_trace(rows: Vec<Event>, success: bool) {
    let (observations, _) = project(rows).unwrap();
    let destination = Path::new(SPEC).join(".generated");
    fs::create_dir_all(&destination).unwrap();
    let mut module = tempfile::Builder::new().prefix("test-").suffix(".qnt").tempfile_in(&destination).unwrap();
    module.write_all(render(&observations).as_bytes()).unwrap();
    let result = Command::new(Path::new(SPEC).join("node_modules/.bin/quint"))
        .arg("test")
        .arg(module.path())
        .arg("--max-samples=1")
        .output()
        .expect("install Quint with npm ci --prefix spec --ignore-scripts");
    let output = String::from_utf8_lossy(&result.stdout).into_owned() + &String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.success(), success, "{output}");
    if !success {
        assert!(output.contains("Assertion failed"), "{output}");
    }
}

#[test]
#[ignore = "requires npm ci --prefix spec --ignore-scripts; run in advisory Quint CI"]
fn quint_valid_completion_schedules() {
    for end in ["task.completed", "session.finalized"] {
        check_trace(with_tail(vec![event(4, "task.completed_by_command"), event(5, end)]), true);
    }
}

#[test]
#[ignore = "requires Quint"]
fn quint_rejects_duplicate_dead_completion() {
    check_trace(with_tail(vec![event(4, "task.completed"), event(5, "task.completed")]), false);
}

#[test]
#[ignore = "requires Quint"]
fn quint_rejects_launch_without_start_and_duplicate_launch() {
    let rows = launched();
    check_trace(vec![rows[0].clone(), rows[2].clone()], false);
    check_trace(with_tail(vec![payload(4, "session.launched", json!({"attempt": 1}))]), false);
}

#[test]
#[ignore = "requires Quint"]
fn quint_rejects_wrong_attempt() {
    let mut rows = launched();
    rows[2].data["attempt"] = json!(2);
    check_trace(rows, false);
}

#[test]
#[ignore = "requires Quint"]
fn quint_checks_backoff_boundary() {
    for (seconds, success) in [(5, false), (13, false), (14, true), (15, true)] {
        let restart = Event { timestamp: event(seconds, "").timestamp, ..payload(5, "task.starting", json!({"attempt": 2})) };
        check_trace(with_tail(vec![crash(3), restart]), success);
    }
}

#[test]
#[ignore = "requires Quint"]
fn quint_checks_recorded_crash_deadline_despite_delayed_logging() {
    let mut row = crash(3);
    row.data["retry_at"] = json!(event(13, "").timestamp);
    for (seconds, success) in [(12, false), (13, true), (14, true)] {
        let restart = Event { timestamp: event(seconds, "").timestamp, ..payload(5, "task.starting", json!({"attempt": 2})) };
        check_trace(with_tail(vec![row.clone(), restart]), success);
    }
}

#[test]
#[ignore = "requires Quint"]
fn quint_checks_failure_attempt_limit() {
    check_trace(with_tail(vec![payload(4, "task.failed", json!({"attempt": 1, "max_attempts": 3}))]), false);
    check_trace(with_tail(vec![crash(1)]), false);
    check_trace(with_tail(vec![payload(4, "task.failed", json!({"attempt": 1, "max_attempts": 1}))]), true);
}

#[test]
#[ignore = "requires Quint"]
fn quint_rate_limit_retains_session_and_checks_wake_time() {
    let limit = payload(4, "budget.rate_limited", json!({"until": event(20, "").timestamp}));
    let restart = Event { timestamp: event(30, "").timestamp, ..payload(5, "task.starting", json!({"attempt": 2})) };
    check_trace(with_tail(vec![limit.clone(), restart]), false);
    for (seconds, success) in [(19, false), (20, true)] {
        let wake = Event {
            timestamp: event(seconds, "").timestamp,
            message: "rate-limit cooldown passed; session still alive".into(),
            ..event(5, "task.resumed")
        };
        check_trace(with_tail(vec![limit.clone(), wake]), success);
    }
}

#[test]
#[ignore = "requires Quint"]
fn quint_rejects_duplicate_creation_but_accepts_source_echoes() {
    check_trace(with_tail(vec![event(4, "task.created")]), false);
    let mut rows = launched();
    rows[0].data = json!({"identifier": "EXAMPLE-1"});
    rows.extend([
        event(4, "task.created"),
        payload(5, "task.cancelled", json!({"linear_state_type": "canceled"})),
        Event { message: "issue closed in Linear".into(), ..event(6, "task.cancelled") },
        event(7, "session.finalized"),
    ]);
    check_trace(rows, true);
}

fn github_created() -> Event {
    payload(1, "task.created", json!({"source":"github"}))
}

fn admission(index: i64, approved: bool) -> Event {
    payload(index, if approved { "github.admitted" } else { "github.revoked" }, json!({"admission":{"approved":approved}}))
}

#[test]
fn github_admission_is_projected_and_unmapped_github_events_are_rejected() {
    let rows = vec![
        github_created(),
        admission(2, true),
        payload(3, "task.starting", json!({"attempt":1})),
        admission(4, false),
        event(5, "github.launch_held"),
        admission(6, true),
        payload(7, "task.starting", json!({"attempt":2})),
        payload(8, "session.launched", json!({"attempt":2})),
    ];
    let (observations, report) = project(rows).unwrap();
    assert_eq!(
        observations.iter().map(|o| o.op).collect::<Vec<_>>(),
        [Op::CreateGitHub, Op::Approve, Op::Start, Op::Revoke, Op::Hold, Op::Approve, Op::Start, Op::Launch]
    );
    assert!(report.stutters.is_empty());
    for kind in ["github.teleported", "github.admission_unknown"] {
        rejects(with_tail(vec![event(4, kind)]), "unmapped kind");
    }
    rejects(with_tail(vec![event(4, "github.admitted")]), "missing approval decision");
    rejects(with_tail(vec![payload(4, "github.admitted", json!({"admission":{"approved":false}}))]), "inconsistent approval");
}

#[test]
#[ignore = "requires Quint"]
fn quint_rejects_unapproved_github_starts_and_launches() {
    check_trace(
        vec![
            github_created(),
            payload(2, "task.starting", json!({"attempt":1})),
            payload(3, "session.launched", json!({"attempt":1})),
        ],
        false,
    );
    // Valid before setup, revoked before the agent launch.
    check_trace(
        vec![
            github_created(),
            admission(2, true),
            payload(3, "task.starting", json!({"attempt":1})),
            admission(4, false),
            payload(5, "session.launched", json!({"attempt":1})),
        ],
        false,
    );
    // Local pause/resume/retry cannot restore remote approval.
    check_trace(
        vec![
            github_created(),
            admission(2, true),
            admission(3, false),
            event(4, "task.paused"),
            event(5, "task.resumed"),
            event(6, "task.cancelled"),
            event(7, "task.retried"),
            payload(8, "task.starting", json!({"attempt":1})),
            payload(9, "session.launched", json!({"attempt":1})),
        ],
        false,
    );
}

#[test]
#[ignore = "requires Quint"]
fn quint_accepts_reapproval_and_preserves_active_work_on_revocation() {
    check_trace(
        vec![
            github_created(),
            admission(2, true),
            payload(3, "task.starting", json!({"attempt":1})),
            admission(4, false),
            event(5, "github.launch_held"),
            admission(6, true),
            payload(7, "task.starting", json!({"attempt":2})),
            payload(8, "session.launched", json!({"attempt":2})),
            admission(9, false),
            event(10, "task.completed"),
        ],
        true,
    );
}

fn routing(index: i64, permitted: bool) -> Event {
    payload(index, if permitted { "routing.selected" } else { "routing.blocked" }, json!({"routing":{"permitted":permitted}}))
}

#[test]
fn routing_events_are_explicit_and_decisions_are_validated() {
    let rows = vec![
        github_created(),
        routing(2, true),
        admission(3, true),
        payload(4, "task.starting", json!({"attempt":1})),
        routing(5, false),
        event(6, "routing.launch_held"),
        routing(7, true),
        payload(8, "task.starting", json!({"attempt":2})),
        payload(9, "session.launched", json!({"attempt":2})),
    ];
    let (observations, report) = project(rows).unwrap();
    assert_eq!(
        observations.iter().map(|o| o.op).collect::<Vec<_>>(),
        [
            Op::CreateGitHub,
            Op::RouteAllow,
            Op::Approve,
            Op::Start,
            Op::RouteBlock,
            Op::Hold,
            Op::RouteAllow,
            Op::Start,
            Op::Launch,
        ]
    );
    assert!(report.stutters.is_empty());
    rejects(with_tail(vec![event(4, "routing.selected")]), "missing routing decision");
    rejects(with_tail(vec![payload(4, "routing.blocked", json!({"routing":{"permitted":true}}))]), "inconsistent routing");
    rejects(with_tail(vec![event(4, "routing.unknown")]), "unmapped kind");
}

#[test]
fn production_routing_effects_are_projected_without_discarding_holds() {
    use powerqueue::{
        config::Config,
        domain::{Task, TaskSource},
        routing::RoutingDecision,
        scheduler::transitions::{Effect, on_routing},
    };
    let mut task = Task::new("test", "test", TaskSource::Manual);
    let mut cfg = Config::default();
    cfg.repo.path = "/repo".into();
    let mut rows = launched();
    for decision in [cfg.route(&task), RoutingDecision::blocked("conflicting labels")] {
        for effect in on_routing(&mut task, decision, false) {
            if let Effect::Log { kind, data, .. } = effect {
                rows.push(payload(rows.len() as i64 + 1, &kind, data));
            }
        }
    }
    let (observations, _) = project(rows).unwrap();
    assert_eq!(
        observations.iter().map(|o| o.op).collect::<Vec<_>>(),
        [Op::Create, Op::Start, Op::Launch, Op::RouteAllow, Op::RouteBlock]
    );
}

#[test]
#[ignore = "requires Quint"]
fn quint_keeps_routing_and_source_approval_independent() {
    // Neither a selected route nor fresh approval may repair the other prerequisite.
    for rows in [
        vec![github_created(), routing(2, true)],
        vec![github_created(), routing(2, false), admission(3, true)],
        vec![github_created(), admission(2, true), routing(3, false), admission(4, true)],
    ] {
        let mut rows = rows;
        let next = rows.len() as i64 + 1;
        rows.push(payload(next, "task.starting", json!({"attempt":1})));
        rows.push(payload(next + 1, "session.launched", json!({"attempt":1})));
        check_trace(rows, false);
    }
    for github in [false, true] {
        let created = if github { github_created() } else { event(1, "task.created") };
        let rows = vec![
            created,
            admission(2, true),
            routing(3, true),
            payload(4, "task.starting", json!({"attempt":1})),
            routing(5, false),
            payload(6, "session.launched", json!({"attempt":1})),
        ];
        check_trace(rows, false);
    }
}

#[test]
#[ignore = "requires Quint"]
fn quint_routing_holds_preserve_setup_and_active_work_and_survive_retry() {
    check_trace(
        vec![
            github_created(),
            admission(2, true),
            routing(3, true),
            payload(4, "task.starting", json!({"attempt":1})),
            routing(5, false),
            event(6, "routing.launch_held"),
            routing(7, true),
            payload(8, "task.starting", json!({"attempt":2})),
            payload(9, "session.launched", json!({"attempt":2})),
            routing(10, false),
            event(11, "task.completed"),
        ],
        true,
    );
    check_trace(
        vec![
            event(1, "task.created"),
            routing(2, false),
            event(3, "task.cancelled"),
            event(4, "task.retried"),
            payload(5, "task.starting", json!({"attempt":1})),
            payload(6, "session.launched", json!({"attempt":1})),
        ],
        false,
    );
}
