//! `powerqueue budget ...`.

use anyhow::Result;

use crate::cli::{Context, BudgetCommand};

pub fn run(ctx: &mut Context, cmd: BudgetCommand) -> Result<i32> {
    let _ = (ctx, cmd);
    todo!("TODO: implement `powerqueue budget`")
}
