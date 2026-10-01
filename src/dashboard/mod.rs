//! Live terminal dashboard (ratatui).
//!
//! Read-only against the database except for the few commands it offers
//! (pause/resume/cancel/retry/add/attach), which go through the `commands`
//! table like the CLI does. Layout:
//!
//! ```text
//! ┌ powerqueue ─ daemon ● running ─ 2/2 slots ─ period 43% (day 3.1/7) ────┐
//! │ tasks table: key | state | crit | model | tokens | cpu/rss | age | title │
//! │ budget gauges per tier     │ selected task: timeline / last output       │
//! │ footer: keys                                                              │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```

pub mod app;
pub mod ui;

pub use app::{DashboardApp, Snapshot};

use anyhow::Result;

use crate::config::Config;
use crate::paths::Paths;
use crate::store::Store;

/// Run the dashboard until the user quits.
pub fn run(cfg: &Config, paths: &Paths, store: &Store) -> Result<()> {
    let _ = (cfg, paths, store);
    todo!("TODO(agent-ux)")
}
