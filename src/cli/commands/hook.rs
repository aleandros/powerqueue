//! `powerqueue hook`.
//!
//! Runs inside a Claude Code hook, so it never writes to stdout (Claude Code
//! may interpret hook output) and never fails: problems go to stderr and the
//! exit code is 0 regardless.

use anyhow::Result;

use crate::cli::{Context, HookArgs};
use crate::domain::{HookEvent, TaskId};

pub fn run(ctx: &mut Context, args: HookArgs) -> Result<i32> {
    match run_inner(ctx, &args) {
        Ok(code) => Ok(code),
        Err(e) => {
            eprintln!("powerqueue hook: {e:#}");
            Ok(0)
        }
    }
}

fn run_inner(ctx: &mut Context, args: &HookArgs) -> Result<i32> {
    let event: HookEvent = args.event.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    let session_id = match &args.session {
        Some(s) => Some(uuid::Uuid::parse_str(s.trim()).map_err(|e| anyhow::anyhow!("bad --session `{s}`: {e}"))?),
        None => None,
    };
    let store = ctx.store()?.clone();
    let task_id = match args.task.trim().parse::<TaskId>() {
        Ok(id) => id,
        Err(_) => store.find_task(&args.task)?.map(|t| t.id).ok_or_else(|| anyhow::anyhow!("no task matches `{}`", args.task))?,
    };
    let mut stdin = std::io::stdin().lock();
    crate::hook::handle(&store, task_id, session_id, event, &mut stdin)
}
