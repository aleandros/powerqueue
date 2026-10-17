//! Project event JSON to an anonymized deterministic Quint lifecycle test.
//! Run with `cargo run --example quint-replay -- events.json --run`.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Timelike, Utc};
use clap::Parser;
use powerqueue::domain::{Event, TaskId};
use regex::Regex;
use serde::Serialize;

const SPEC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/spec");
const STUTTERS: &[&str] = &[
    // Circuit events do not change task lifecycle; pass admission is not replayed.
    "launch.failure",
    "launch.claim_superseded",
    "launch.recovered",
    "task.updated",
    "task.models_changed",
    "task.project_bound",
    "task.project_set",
    "task.model_set",
    "task.model_override",
    "task.retry_ignored",
    "session.started",
    "session.turn_ended",
    "session.turn_failed",
    "session.idle_prompt",
    "session.permission_prompt",
    "session.nudged",
    "session.stale",
    "session.timeout",
    "session.discovered",
    "session.discovery_failed",
    "review.lookup_failed",
    "review.status",
    "review.hold",
    // Typing a hint does not change task/session state; answers can, and
    // remain prefix cutoffs along with the other unsupported relay events.
    "relay.hint_sent",
    "session.marker_polled",
    "session.rate_limit_detected",
    "github.sync",
    "github.label",
    "github.closed",
    "github.comment",
    "github.error",
    "github.unavailable",
    "github.update_failed",
    "github.update_skipped",
];
const STUTTER_PREFIXES: &[&str] =
    &["hook.", "worktree.", "cleanup.", "linear.", "repo.", "daemon.", "rules.", "tune.", "config.", "usage."];
const OUT_OF_SCOPE: &[&str] = &[
    "task.skipped",
    "task.unskipped",
    "task.needs_attention",
    "task.error",
    "launch.superseded",
    "session.exited",
    "task.blocked_by_command",
    "session.permission_prompt",
    "session.ended",
];
const REVIEW_ACTIONS: &[&str] = &["review.merged", "review.relaunch", "review.closed", "review.stale"];

type Counts = BTreeMap<String, usize>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum Op {
    Create,
    CreateGitHub,
    Approve,
    Revoke,
    RouteAllow,
    RouteBlock,
    Hold,
    Start,
    Launch,
    Crash,
    Fail,
    Finish,
    Complete,
    Release,
    Throttle,
    RateLimit,
    Pause,
    Resume,
    Cancel,
    SourceCancel,
    Retry,
    Progress,
    Wake,
    DependencyBlock,
    Unblock,
    HandOff,
    ReviewRelease,
    ReviewRetry,
    ReviewPark,
    Merge,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Observation {
    id: usize,
    op: Op,
    time: i64,
    until: i64,
    attempt: i64,
    maximum: i64,
    // Only fixed, validated state names; never arbitrary event text.
    previous_state: &'static str,
    event: i64,
}

#[derive(Debug, Serialize)]
struct Coverage {
    events: usize,
    tasks: usize,
    truncated_or_skipped_tasks: Counts,
    observations: usize,
    actions: Counts,
    stutters: Counts,
    first_timestamp: DateTime<Utc>,
    last_timestamp: DateTime<Utc>,
}

fn count(counts: &mut Counts, key: impl Into<String>) {
    *counts.entry(key.into()).or_default() += 1;
}

// Preserve the original adapter's microsecond truncation before converting to
// coarse seconds. Comparing full-precision instants here would change schedules
// with nanosecond timestamps at second boundaries.
fn instant(time: DateTime<Utc>) -> DateTime<Utc> {
    time.with_nanosecond(time.nanosecond() / 1_000 * 1_000).expect("truncated nanoseconds remain valid")
}

fn seconds(time: DateTime<Utc>, origin: DateTime<Utc>) -> i64 {
    (instant(time) - origin).num_seconds()
}

fn positive(event: &Event, key: &str) -> Result<i64> {
    event.data.get(key).and_then(|v| v.as_i64()).filter(|v| *v >= 1).with_context(|| format!("event {}: invalid {key}", event.id))
}

fn deadline(event: &Event, key: &str, origin: DateTime<Utc>) -> Result<i64> {
    let value =
        event.data.get(key).and_then(|v| v.as_str()).with_context(|| format!("event {}: missing {key} timestamp", event.id))?;
    // Do not include arbitrary event payloads in error output.
    let time = DateTime::parse_from_rfc3339(value)
        .map_err(|_| anyhow::anyhow!("event {}: invalid {key} timestamp (timezone required)", event.id))?;
    Ok(seconds(time.with_timezone(&Utc), origin))
}

// `task.blocked` is overloaded: dependency reconciliation includes
// previous_state; an agent asking for help includes reason instead. Only the
// former is modeled for non-container tasks. Never continue past an unsupported
// human blocker, container completion, or automatic PR adoption.
fn ends_prefix(event: &Event) -> bool {
    let kind = event.kind.as_str();
    OUT_OF_SCOPE.contains(&kind)
        || (kind == "task.blocked"
            && (event.data.get("previous_state").is_none()
                || event.data.get("container").and_then(|v| v.as_bool()) == Some(true)))
        || (kind == "task.completed" && event.data.get("children").is_some())
        || (kind == "task.in_review" && event.data.get("adopted").and_then(|v| v.as_bool()) == Some(true))
        || (["review.", "relay.", "inbox."].iter().any(|p| kind.starts_with(p))
            && !STUTTERS.contains(&kind)
            && !REVIEW_ACTIONS.contains(&kind))
}

/// Project complete task prefixes; refuse malformed data and vacuous schedules.
fn project(mut events: Vec<Event>) -> Result<(Vec<Observation>, Coverage)> {
    ensure!(!events.is_empty(), "expected a nonempty JSON array from logs --events --json");
    events.sort_by_key(|e| e.id);
    ensure!(!events.windows(2).any(|w| w[0].id == w[1].id), "duplicate event IDs");
    let origin = events.iter().map(|e| instant(e.timestamp)).min().expect("nonempty events");
    let mut grouped: HashMap<TaskId, Vec<&Event>> = HashMap::new();
    let mut order = Vec::new();
    for event in &events {
        if let Some(task) = event.task_id {
            let rows = grouped.entry(task).or_insert_with(|| {
                order.push(task);
                Vec::new()
            });
            rows.push(event);
        }
    }
    let mut skipped = Counts::new();
    let mut eligible = HashMap::new();
    let mut cutoff = HashMap::new();
    for task in order {
        let rows = &grouped[&task];
        if rows[0].kind != "task.created" {
            count(&mut skipped, "missing initial creation (partial history)");
            continue;
        }
        eligible.insert(task, eligible.len());
        for row in rows {
            let kind = row.kind.as_str();
            if ends_prefix(row) {
                cutoff.insert(task, row.id);
                count(&mut skipped, format!("prefix ends at {kind}"));
                break;
            }
        }
    }
    let backoff = Regex::new(r"; retrying in (\d+)s \(attempt ").expect("constant crash backoff regex");
    let mut observations = Vec::new();
    let mut creations = HashMap::new();
    let mut cancellations = HashSet::new();
    let mut ignored = Counts::new();
    let mut last_time = 0;
    for event in &events {
        let Some(task) = event.task_id else { continue };
        let Some(&id) = eligible.get(&task) else { continue };
        if cutoff.get(&task).is_some_and(|cutoff| event.id >= *cutoff) {
            continue;
        }
        let kind = event.kind.as_str();
        let data = &event.data;
        let empty = data.as_object().is_some_and(|o| o.is_empty());
        let time = seconds(event.timestamp, origin);
        ensure!(time >= last_time, "event {}: clock moved backwards", event.id);
        last_time = time;
        let mut until = -1;
        let mut attempt = -1;
        let mut maximum = 0;
        let mut previous_state = "";
        let op = match kind {
            "task.created" => {
                if empty && creations.get(&task) == Some(&true) {
                    creations.insert(task, false);
                    count(&mut ignored, "task.created (daemon echo of Linear sync)");
                    continue;
                }
                creations.insert(task, data.get("identifier").is_some());
                if data.get("source").and_then(|v| v.as_str()) == Some("github") { Op::CreateGitHub } else { Op::Create }
            }
            "budget.rate_limited" if data.get("source").and_then(|v| v.as_str()) == Some("pane") => {
                count(&mut ignored, "budget.rate_limited (provider only)");
                continue;
            }
            "task.cancelled" if empty && event.message == "issue closed in Linear" && cancellations.remove(&task) => {
                count(&mut ignored, "task.cancelled (daemon echo of Linear sync)");
                continue;
            }
            "github.admitted" | "github.revoked" | "github.admission_blocked" => {
                let approved = data
                    .pointer("/admission/approved")
                    .and_then(|v| v.as_bool())
                    .with_context(|| format!("event {}: missing approval decision", event.id))?;
                ensure!(approved == (kind == "github.admitted"), "event {}: inconsistent approval decision", event.id);
                if approved { Op::Approve } else { Op::Revoke }
            }
            "routing.selected" | "routing.blocked" => {
                let permitted = data
                    .pointer("/routing/permitted")
                    .and_then(|v| v.as_bool())
                    .with_context(|| format!("event {}: missing routing decision", event.id))?;
                ensure!(permitted == (kind == "routing.selected"), "event {}: inconsistent routing decision", event.id);
                if permitted { Op::RouteAllow } else { Op::RouteBlock }
            }
            "github.launch_held" | "routing.launch_held" => Op::Hold,
            "task.starting" => Op::Start,
            "session.launched" => Op::Launch,
            "session.crashed" => Op::Crash,
            "task.failed" => Op::Fail,
            "task.completed" => Op::Finish,
            "task.completed_by_command" => Op::Complete,
            "session.finalized" => Op::Release,
            "task.blocked" => {
                previous_state = match data.get("previous_state").and_then(|v| v.as_str()) {
                    Some("queued") => "queued",
                    Some("throttled") => "throttled",
                    _ => bail!("event {}: invalid dependency block payload", event.id),
                };
                ensure!(
                    data.get("container").and_then(|v| v.as_bool()) == Some(false)
                        && data.get("waiting_on").and_then(|v| v.as_array()).is_some(),
                    "event {}: invalid dependency block payload",
                    event.id
                );
                Op::DependencyBlock
            }
            "task.unblocked" => Op::Unblock,
            "task.in_review" => Op::HandOff,
            "session.released" => Op::ReviewRelease,
            "review.relaunch" => Op::ReviewRetry,
            "review.merged" => Op::Merge,
            "review.closed" | "review.stale" => Op::ReviewPark,
            "task.throttled" => Op::Throttle,
            "budget.rate_limited" | "session.waiting_for_reset" => Op::RateLimit,
            "task.paused" => Op::Pause,
            "task.resumed" if event.message == "rate-limit cooldown passed; session still alive" => Op::Wake,
            "task.resumed" => Op::Resume,
            "task.cancelled" if data.get("linear_state_type").is_some() => {
                cancellations.insert(task);
                Op::SourceCancel
            }
            "task.cancelled" if data.get("source").and_then(|v| v.as_str()) == Some("github") => Op::SourceCancel,
            "task.cancelled" => Op::Cancel,
            "task.retried" => Op::Retry,
            "task.attention_resolved" => Op::Progress,
            _ if STUTTERS.contains(&kind) || STUTTER_PREFIXES.iter().any(|p| kind.starts_with(p)) => {
                count(&mut ignored, kind);
                continue;
            }
            _ => bail!("event {}: unmapped kind {kind:?}; classify explicitly", event.id),
        };
        if matches!(op, Op::Start | Op::Launch | Op::Crash | Op::Fail) {
            attempt = positive(event, "attempt")?;
        }
        if matches!(op, Op::Crash | Op::Fail) {
            maximum = positive(event, "max_attempts")?;
        }
        match op {
            // The transition's clock precedes log persistence. Adding backoff
            // to the log timestamp can falsely reject an on-time restart.
            Op::Crash if event.data.get("retry_at").is_some() => until = deadline(event, "retry_at", origin)?,
            Op::Crash => {
                let captures =
                    backoff.captures(&event.message).with_context(|| format!("event {}: missing crash backoff", event.id))?;
                let delay: i64 = captures[1].parse().with_context(|| format!("event {}: invalid crash backoff", event.id))?;
                until = time.checked_add(delay).with_context(|| format!("event {}: crash backoff overflow", event.id))?;
            }
            Op::Throttle => until = deadline(event, "retry_at", origin)?,
            Op::RateLimit => until = deadline(event, "until", origin)?,
            _ => {}
        }
        observations.push(Observation { id, op, time, until, attempt, maximum, previous_state, event: event.id });
    }
    ensure!(observations.iter().any(|o| o.op == Op::Launch), "no in-scope launches; refusing a vacuous replay");
    let mut actions = Counts::new();
    for observation in &observations {
        count(&mut actions, format!("{:?}", observation.op));
    }
    let report = Coverage {
        events: events.len(),
        tasks: eligible.len(),
        truncated_or_skipped_tasks: skipped,
        observations: observations.len(),
        actions,
        stutters: ignored,
        first_timestamp: events[0].timestamp,
        last_timestamp: events.last().expect("nonempty events").timestamp,
    };
    Ok((observations, report))
}

/// Render only typed numeric fields and fixed operation/state names, never event text.
fn render(observations: &[Observation]) -> String {
    let rows = observations
        .iter()
        .map(|o| {
            format!(
                "    {{ id: {}, op: \"{:?}\", time: {}, until: {}, attempt: {}, maximum: {}, previous_state: \"{}\", event: {} }}",
                o.id, o.op, o.time, o.until, o.attempt, o.maximum, o.previous_state, o.event,
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    include_str!("replay/template.qnt").replace("// OBSERVATIONS", &rows)
}

fn output_name(value: &str) -> Result<String, String> {
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        Ok(value.into())
    } else {
        Err("--name must contain only letters, numbers, underscore or hyphen".into())
    }
}

#[derive(Parser)]
#[command(about = "Project event JSON to an anonymized deterministic Quint lifecycle test")]
struct Args {
    events: PathBuf,
    /// Local output basename under spec/.generated.
    #[arg(long, default_value = "replay", value_parser = output_name)]
    name: String,
    /// Run Quint and emit validated ITF (or a counterexample).
    #[arg(long)]
    run: bool,
}

fn rejected_observation(trace: &Path, observations: &[Observation]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(trace).ok()?).ok()?;
    let state = value.get("states")?.as_array()?.iter().rev().find(|s| s.get("index").is_some())?;
    let position: usize = state.get("index")?.get("#bigint")?.as_str()?.parse().ok()?;
    serde_json::to_string(observations.get(position)?).ok()
}

fn run(args: Args) -> Result<()> {
    let input = fs::read(&args.events).with_context(|| format!("read {}", args.events.display()))?;
    let events: Vec<Event> =
        serde_json::from_slice(&input).with_context(|| format!("parse event array in {}", args.events.display()))?;
    let (observations, coverage) = project(events)?;
    let destination = Path::new(SPEC).join(".generated");
    fs::create_dir_all(&destination).with_context(|| format!("create {}", destination.display()))?;
    let path = destination.join(format!("{}.qnt", args.name));
    fs::write(&path, render(&observations)).with_context(|| format!("write {}", path.display()))?;
    let report = serde_json::to_string_pretty(&coverage)? + "\n";
    let coverage_path = destination.join(format!("{}.coverage.json", args.name));
    fs::write(&coverage_path, &report).with_context(|| format!("write {}", coverage_path.display()))?;
    print!("{report}");
    io::stdout().flush().context("flush coverage report")?;
    if args.run {
        let trace = destination.join(format!("{}.itf.json", args.name));
        match fs::remove_file(&trace) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("remove {}", trace.display())),
        }
        let quint = Path::new(SPEC).join("node_modules/.bin/quint");
        let status = Command::new(&quint)
            .arg("test")
            .arg(&path)
            .arg("--max-samples=1")
            .arg(format!("--out-itf={}", trace.display()))
            .status()
            .with_context(|| format!("run {}; install with npm ci --prefix spec --ignore-scripts", quint.display()))?;
        if !status.success() {
            if let Some(observation) = rejected_observation(&trace, &observations) {
                eprintln!("Rejected observation: {observation}");
            }
            bail!("Quint replay failed ({status})");
        }
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run(Args::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("replay failed: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[path = "replay/tests.rs"]
mod tests;
