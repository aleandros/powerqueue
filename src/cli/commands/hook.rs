//! `powerqueue hook`.
//!
//! Runs inside an agent CLI hook, so it never writes to stdout (Claude Code
//! may interpret hook output) and never fails: problems go to stderr and the
//! exit code is 0 regardless. `--provider` names the CLI that sent the hook;
//! its payload (stdin, or the trailing argument for Codex `notify`) is
//! normalised to the Claude shape before it is stored.

use anyhow::Result;

use crate::cli::{Context, HookArgs};
use crate::domain::TaskId;

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
    let session_id = match &args.session {
        Some(s) => Some(uuid::Uuid::parse_str(s.trim()).map_err(|e| anyhow::anyhow!("bad --session `{s}`: {e}"))?),
        None => None,
    };
    let store = ctx.store()?.clone();
    let task_id = match args.task.trim().parse::<TaskId>() {
        Ok(id) => id,
        Err(_) => store.find_task(&args.task)?.map(|t| t.id).ok_or_else(|| anyhow::anyhow!("no task matches `{}`", args.task))?,
    };
    let raw = match &args.payload {
        Some(p) => p.clone(),
        None => {
            let mut raw = String::new();
            if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut raw) {
                tracing::warn!(error = %e, "hook: cannot read stdin; storing an empty payload");
            }
            raw
        }
    };
    crate::hook::handle_provider(&store, args.provider, task_id, session_id, &args.event, &raw)
}
