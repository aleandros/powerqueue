//! `powerqueue config ...`.

use anyhow::Result;

use crate::cli::{ConfigCommand, Context};

pub fn run(ctx: &mut Context, cmd: ConfigCommand) -> Result<i32> {
    let _ = (ctx, cmd);
    todo!("TODO: implement `powerqueue config`")
}
