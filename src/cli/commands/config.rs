//! `powerqueue config ...`.

use anyhow::Result;

use crate::cli::{Context, ConfigCommand};

pub fn run(ctx: &mut Context, cmd: ConfigCommand) -> Result<i32> {
    let _ = (ctx, cmd);
    todo!("TODO: implement `powerqueue config`")
}
