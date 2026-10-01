//! `powerqueue priority ...`.

use anyhow::Result;

use crate::cli::{Context, PriorityCommand};

pub fn run(ctx: &mut Context, cmd: PriorityCommand) -> Result<i32> {
    let _ = (ctx, cmd);
    todo!("TODO: implement `powerqueue priority`")
}
