//! `powerqueue doctor`.

use anyhow::Result;

use crate::cli::{Context, DoctorArgs};

pub fn run(ctx: &mut Context, args: DoctorArgs) -> Result<i32> {
    let _ = (ctx, args);
    todo!("TODO: implement `powerqueue doctor`")
}
