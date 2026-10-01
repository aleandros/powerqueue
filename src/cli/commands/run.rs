//! `powerqueue run` / `powerqueue stop`.

use anyhow::Result;

use crate::cli::{Context, RunArgs};

pub fn run(ctx: &mut Context, args: RunArgs) -> Result<i32> {
    let _ = (ctx, args);
    todo!("TODO(agent-budget): build Daemon, runtime().block_on(daemon.run(args.once))")
}

pub fn stop(ctx: &mut Context) -> Result<i32> {
    let _ = ctx;
    todo!("TODO(agent-budget): enqueue DaemonCommand::Shutdown; report if no daemon heartbeat")
}
