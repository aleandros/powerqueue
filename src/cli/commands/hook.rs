//! `powerqueue hook`.
//!
//! Runs inside an agent CLI hook, so it never fails: problems go to stderr
//! and the exit code is 0 regardless. It writes nothing to stdout (Claude
//! Code may interpret hook output) except for `--event StatusLine`, whose
//! stdout Claude Code shows as the status line.
//!
//! `--provider` names the CLI that sent the hook; its payload (stdin, or the
//! trailing argument for Codex `notify`) goes through
//! `agent_for(provider).normalize_hook` so the stored event is Claude-shaped.
//! An event the provider does not map is ignored.

use anyhow::Result;
use chrono::Utc;

use crate::budget::probes::claude::STATUS_LINE_EVENT;
use crate::cli::{Context, HookArgs};
use crate::domain::{Provider, TaskId};
use crate::session::agent_for;

pub fn run(ctx: &mut Context, args: HookArgs) -> Result<i32> {
    if args.provider == Provider::Claude && args.event.trim().eq_ignore_ascii_case(STATUS_LINE_EVENT) {
        return Ok(status_line(ctx, &args));
    }
    match run_inner(ctx, &args) {
        Ok(code) => Ok(code),
        Err(e) => {
            eprintln!("powerqueue hook: {e:#}");
            Ok(0)
        }
    }
}

/// `--event StatusLine`: store the rate limits, print the status text.
fn status_line(ctx: &mut Context, args: &HookArgs) -> i32 {
    let store = match ctx.store() {
        Ok(s) => Some(s.clone()),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "status line: cannot open the database");
            None
        }
    };
    let text = match &args.payload {
        Some(raw) => crate::hook::handle_status_line(store.as_ref(), &mut raw.as_bytes(), Utc::now()),
        None => crate::hook::handle_status_line(store.as_ref(), &mut std::io::stdin().lock(), Utc::now()),
    };
    println!("{text}");
    0
}

fn run_inner(ctx: &mut Context, args: &HookArgs) -> Result<i32> {
    let session_id = match &args.session {
        Some(s) => Some(uuid::Uuid::parse_str(s.trim()).map_err(|e| anyhow::anyhow!("bad --session `{s}`: {e}"))?),
        None => None,
    };
    let store = ctx.store()?.clone();
    let task_id = match args.task.trim().parse::<TaskId>() {
        Ok(id) => id,
        Err(_) => store.find_task(&args.task)?.map(|t| t.id).ok_or_else(|| anyhow::anyhow!("no task matches `{}`", args.task))?,
    };
    let payload = match &args.payload {
        Some(raw) => crate::hook::parse_payload(raw),
        None => crate::hook::read_payload(&mut std::io::stdin().lock()),
    };
    let Some((event, payload)) = agent_for(args.provider).normalize_hook(args.event.trim(), payload) else {
        if args.provider == Provider::Claude {
            anyhow::bail!("unknown hook event `{}`", args.event);
        }
        tracing::debug!(provider = %args.provider, event = %args.event, "hook event ignored");
        return Ok(0);
    };
    crate::hook::handle_payload(&store, task_id, session_id, event, payload)
}
