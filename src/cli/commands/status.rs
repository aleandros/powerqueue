//! `powerqueue status`.

use anyhow::Result;

use crate::cli::{Context, StatusArgs};

pub fn run(ctx: &mut Context, args: StatusArgs) -> Result<i32> {
    let _ = (ctx, args);
    todo!("TODO: implement `powerqueue status`")
}
