//! `powerqueue linear ...`.

use anyhow::Result;

use crate::cli::{Context, LinearCommand};

pub fn run(ctx: &mut Context, cmd: LinearCommand) -> Result<i32> {
    let _ = (ctx, cmd);
    todo!("TODO: implement `powerqueue linear`")
}
