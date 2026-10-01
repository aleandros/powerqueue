//! `powerqueue hook`.

use anyhow::Result;

use crate::cli::{Context, HookArgs};

pub fn run(ctx: &mut Context, args: HookArgs) -> Result<i32> {
    let _ = (ctx, args);
    todo!("TODO: implement `powerqueue hook`")
}
