//! `powerqueue linear ...`: teams, states, key test and a (dry-run) sync.

use std::collections::HashSet;

use anyhow::{Context as _, Result};
use owo_colors::{OwoColorize, Stream};
use tracing::warn;

use crate::cli::output::{table, truncate};
use crate::cli::{Context, LinearCommand};
use crate::domain::TaskState;
use crate::linear::sync::apply_issue;
use crate::linear::{IssueFilter, LinearClient, LinearIssue, sync_issues};
use crate::secrets::SecretKind;

/// Dispatch a `linear` subcommand. Returns the exit code.
pub fn run(ctx: &mut Context, cmd: LinearCommand) -> Result<i32> {
    match cmd {
        LinearCommand::Teams => teams(ctx),
        LinearCommand::States { team } => states(ctx, &team),
        LinearCommand::Test => test(ctx),
        LinearCommand::Sync { apply } => sync(ctx, apply),
    }
}

/// Build a client from the configured endpoint and the stored key.
fn client(ctx: &mut Context) -> Result<LinearClient> {
    let endpoint = ctx.config_or_default()?.linear.endpoint.clone();
    let key = ctx.secrets().require(SecretKind::LinearApiKey)?;
    LinearClient::new(endpoint, key)
}

fn teams(ctx: &mut Context) -> Result<i32> {
    let client = client(ctx)?;
    let rt = super::runtime()?;
    let teams = rt.block_on(client.teams())?;
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&teams)?);
        return Ok(0);
    }
    if teams.is_empty() {
        println!("No teams are visible to this API key.");
        return Ok(0);
    }
    let mut t = table();
    t.set_header(["KEY", "NAME", "ID"]);
    for team in &teams {
        t.add_row([team.key.as_str(), team.name.as_str(), team.id.as_str()]);
    }
    println!("{t}");
    Ok(0)
}

fn states(ctx: &mut Context, team: &str) -> Result<i32> {
    let client = client(ctx)?;
    let rt = super::runtime()?;
    let states = rt.block_on(client.workflow_states(team))?;
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&states)?);
        return Ok(0);
    }
    if states.is_empty() {
        println!("No workflow states found for team `{team}` (check the key with `powerqueue linear teams`).");
        return Ok(1);
    }
    let mut t = table();
    t.set_header(["NAME", "TYPE", "ID"]);
    for s in &states {
        t.add_row([s.name.as_str(), s.kind.as_str(), s.id.as_str()]);
    }
    println!("{t}");
    Ok(0)
}

fn test(ctx: &mut Context) -> Result<i32> {
    let endpoint = ctx.config_or_default()?.linear.endpoint.clone();
    let (key, origin) = ctx
        .secrets()
        .get_with_origin(SecretKind::LinearApiKey)?
        .ok_or_else(|| anyhow::anyhow!("Linear API key not configured. Run `powerqueue init` or set LINEAR_API_KEY."))?;
    let client = LinearClient::new(&endpoint, key)?;
    let rt = super::runtime()?;
    let viewer = rt.block_on(client.viewer()).with_context(|| format!("Linear API test against {endpoint} failed"))?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true, "viewer": viewer, "key_origin": origin.to_string(), "endpoint": endpoint,
            }))?
        );
    } else {
        println!(
            "{} authenticated as {} <{}> (key from {origin}, endpoint {endpoint})",
            "ok:".if_supports_color(Stream::Stdout, |t| t.style(owo_colors::Style::new().green().bold())),
            viewer.name,
            viewer.email
        );
    }
    Ok(0)
}

/// One row of the dry-run table.
#[derive(Debug, serde::Serialize)]
struct Planned {
    action: &'static str,
    key: String,
    title: String,
    detail: String,
}

fn sync(ctx: &mut Context, apply: bool) -> Result<i32> {
    let cfg = ctx.config_cloned()?;
    let key = ctx.secrets().require(SecretKind::LinearApiKey)?;
    let client = LinearClient::new(&cfg.linear.endpoint, key)?;
    let rt = super::runtime()?;
    let filter = IssueFilter::from_config(&cfg.linear);
    let mut issues = rt.block_on(client.fetch_issues(&filter)).context("fetch issues from Linear")?;
    let store = ctx.store()?.clone();
    rt.block_on(add_dependencies(&client, &store, &mut issues))?;

    // Resolve the state type of an issue that left the queue. Errors must not
    // cancel anything, so they read as "still open".
    let fetch_state = |issue_id: &str| -> Option<String> {
        match rt.block_on(client.get_issue(issue_id)) {
            Ok(Some(issue)) => Some(issue.state_type),
            Ok(None) => None,
            Err(e) => {
                warn!(target: "powerqueue::linear", issue = %issue_id, error = %e, "could not fetch issue state; leaving task alone");
                Some("unknown".to_string())
            }
        }
    };

    if apply {
        let report = sync_issues(&store, &cfg.linear, &issues, fetch_state)?;
        let keys = |ids: &[crate::domain::TaskId]| -> Vec<String> {
            ids.iter().filter_map(|id| store.get_task(*id).ok().flatten()).map(|t| t.key).collect()
        };
        let created = keys(&report.created);
        let updated = keys(&report.updated);
        let cancelled = keys(&report.cancelled);
        if ctx.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "applied": true, "fetched": issues.len(),
                    "created": created, "updated": updated, "cancelled": cancelled, "unchanged": report.unchanged,
                }))?
            );
            return Ok(0);
        }
        println!(
            "Fetched {} issue(s); created {}, updated {}, cancelled {}, unchanged {}.",
            issues.len(),
            created.len().if_supports_color(Stream::Stdout, |t| t.green()),
            updated.len().if_supports_color(Stream::Stdout, |t| t.yellow()),
            cancelled.len().if_supports_color(Stream::Stdout, |t| t.red()),
            report.unchanged
        );
        for (what, list) in [("created", &created), ("updated", &updated), ("cancelled", &cancelled)] {
            if !list.is_empty() {
                println!("  {what}: {}", list.join(", "));
            }
        }
        return Ok(0);
    }

    let planned = plan(&store, &issues, fetch_state)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "applied": false, "fetched": issues.len(), "changes": planned,
            }))?
        );
        return Ok(0);
    }
    println!("Dry run: fetched {} queued issue(s) from Linear (filters: {}).", issues.len(), describe_filter(&filter));
    if planned.is_empty() {
        println!("Nothing would change. Re-run with --apply to sync anyway.");
        return Ok(0);
    }
    let mut t = table();
    t.set_header(["ACTION", "KEY", "TITLE", "DETAILS"]);
    for p in &planned {
        let action = match p.action {
            "create" => p.action.if_supports_color(Stream::Stdout, |t| t.green()).to_string(),
            "update" => p.action.if_supports_color(Stream::Stdout, |t| t.yellow()).to_string(),
            "cancel" => p.action.if_supports_color(Stream::Stdout, |t| t.red()).to_string(),
            other => other.to_string(),
        };
        t.add_row([action, p.key.clone(), truncate(&p.title, 60), p.detail.clone()]);
    }
    println!("{t}");
    println!("Re-run with --apply to make these changes.");
    Ok(0)
}

/// Fill in each issue's sub-issues and `blocked by` issues (as the daemon
/// does), so a sync never wipes the stored relations. A blocker's merged PR
/// is kept from the store (merged stays merged) or asked for; a failed
/// lookup leaves the blocker pending.
async fn add_dependencies(client: &LinearClient, store: &crate::store::Store, issues: &mut [LinearIssue]) -> Result<()> {
    let ids: Vec<String> = issues.iter().map(|i| i.id.clone()).collect();
    let deps = client.fetch_dependencies(&ids).await.context("fetch issue relations from Linear")?;
    let merged: std::collections::HashSet<String> = store
        .list_open_tasks()?
        .iter()
        .flat_map(|t| t.blocked_by.iter().filter(|b| b.pr_merged).map(|b| b.key.clone()))
        .collect();
    for issue in issues.iter_mut() {
        let Some(d) = deps.get(&issue.id) else {
            // Not returned (should not happen): keep what is stored.
            if let Some(t) = store.get_task_by_linear_issue(&issue.id)? {
                issue.blocked_by = t.blocked_by;
                issue.children = t.children;
            }
            continue;
        };
        issue.blocked_by = d.blocked_by.clone();
        issue.children = d.children.clone();
        for blocker in issue.blocked_by.iter_mut().filter(|b| !b.is_closed()) {
            blocker.pr_merged = merged.contains(&blocker.key)
                || client.pr_merged(&blocker.key).await.unwrap_or_else(|e| {
                    warn!(target: "powerqueue::linear", blocker = %blocker.key, error = %e, "could not check the blocker's pull request");
                    false
                });
        }
    }
    Ok(())
}

/// Compute what [`sync_issues`] would do without touching the store.
fn plan(
    store: &crate::store::Store,
    issues: &[LinearIssue],
    fetch_state: impl Fn(&str) -> Option<String>,
) -> Result<Vec<Planned>> {
    let mut out = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for issue in issues {
        seen.insert(issue.id.as_str());
        match store.get_task_by_linear_issue(&issue.id)? {
            None => {
                if store.get_task_by_key(&issue.identifier)?.is_none() {
                    out.push(Planned {
                        action: "create",
                        key: issue.identifier.clone(),
                        title: issue.title.clone(),
                        detail: match &issue.cycle {
                            Some(c) => {
                                format!("{} / {} / cycle {} ({})", issue.state_name, issue.labels.join(","), c.number, c.status)
                            }
                            None => format!("{} / {}", issue.state_name, issue.labels.join(",")),
                        },
                    });
                }
            }
            Some(existing) if existing.state.is_terminal() => {}
            Some(mut existing) => {
                let changed = apply_issue(&mut existing, issue);
                if !changed.is_empty() {
                    out.push(Planned {
                        action: "update",
                        key: existing.key,
                        title: issue.title.clone(),
                        detail: changed.join(", "),
                    });
                }
            }
        }
    }
    for task in store.list_open_tasks()? {
        let Some(issue_id) = task.linear_issue_id() else { continue };
        if seen.contains(issue_id) || !(task.state.is_schedulable() || task.state == TaskState::Paused) {
            continue;
        }
        let state_type = fetch_state(issue_id);
        let closed = matches!(
            state_type.as_deref().map(str::to_ascii_lowercase).as_deref(),
            None | Some("completed") | Some("canceled") | Some("cancelled")
        );
        if closed {
            out.push(Planned {
                action: "cancel",
                key: task.key.clone(),
                title: task.title.clone(),
                detail: state_type.map(|s| format!("closed in Linear ({s})")).unwrap_or_else(|| "issue deleted".into()),
            });
        }
    }
    Ok(out)
}

fn describe_filter(f: &IssueFilter) -> String {
    let mut parts = Vec::new();
    if !f.team_keys.is_empty() {
        parts.push(format!("teams {}", f.team_keys.join(",")));
    }
    if !f.state_names.is_empty() {
        parts.push(format!("states {}", f.state_names.join(",")));
    }
    if let Some(a) = &f.assignee {
        parts.push(format!("assignee {a}"));
    }
    if !f.required_labels.is_empty() {
        parts.push(format!("labels {}", f.required_labels.join(",")));
    }
    if !f.excluded_labels.is_empty() {
        parts.push(format!("not {}", f.excluded_labels.join(",")));
    }
    if f.cycle != crate::linear::CycleScope::Any {
        parts.push(format!("cycle {}", f.cycle));
    }
    if !f.projects.is_empty() {
        parts.push(format!("projects {}", f.projects.join(",")));
    }
    if f.max > 0 {
        parts.push(format!("max {}", f.max));
    }
    if parts.is_empty() { "none".to_string() } else { parts.join("; ") }
}
