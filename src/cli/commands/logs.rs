//! `powerqueue logs`.

use anyhow::Result;

use crate::cli::{Context, LogsArgs};

pub fn run(ctx: &mut Context, args: LogsArgs) -> Result<i32> {
    let _ = (ctx, args);
    todo!("TODO: implement `powerqueue logs`")
}
