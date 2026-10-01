//! `powerqueue task ...`.

use anyhow::Result;

use crate::cli::{Context, TaskCommand};

pub fn run(ctx: &mut Context, cmd: TaskCommand) -> Result<i32> {
    let _ = (ctx, cmd);
    todo!("TODO: implement `powerqueue task`")
}
