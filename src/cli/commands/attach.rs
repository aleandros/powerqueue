//! `powerqueue attach`.

use anyhow::Result;

use crate::cli::{Context, AttachArgs};

pub fn run(ctx: &mut Context, args: AttachArgs) -> Result<i32> {
    let _ = (ctx, args);
    todo!("TODO: implement `powerqueue attach`")
}
