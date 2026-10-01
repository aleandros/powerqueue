//! `powerqueue secrets ...`.

use anyhow::Result;

use crate::cli::{Context, SecretsCommand};

pub fn run(ctx: &mut Context, cmd: SecretsCommand) -> Result<i32> {
    let _ = (ctx, cmd);
    todo!("TODO: implement `powerqueue secrets`")
}
