//! `powerqueue dashboard` — preconditions, snapshot mode, then the TUI.
//!
//! The live TUI only starts when the configuration exists, stdin and stdout
//! are terminals and `TERM` is usable; otherwise it fails with a plain error
//! (exit 2 for terminal problems) instead of painting escape sequences into a
//! pipe. `--once` renders a single frame as text (or the snapshot as JSON with
//! `--json`) and works anywhere.

use anyhow::{Result, anyhow};
use chrono::Utc;

use crate::cli::commands::status::ensure_initialised;
use crate::cli::output::print_error;
use crate::cli::{Context, DashboardArgs};
use crate::dashboard::{self, Options, Snapshot, TerminalEnv};

/// Exit code when the dashboard cannot use the terminal it was started from.
pub const EXIT_NO_TTY: i32 = 2;

/// # Panics
/// Panics if snapshot serialization stops representing tasks as a JSON array.
#[expect(
    clippy::needless_pass_by_value,
    reason = "Command dispatch transfers ownership of parsed arguments to the selected handler."
)]
pub fn run(ctx: &mut Context, args: DashboardArgs) -> Result<i32> {
    ensure_initialised(ctx)?;
    let cfg = ctx.config_cloned()?;
    let store = ctx.store()?.clone();
    let env = TerminalEnv::detect();
    let options = Options { ascii: args.ascii || !env.utf8_locale, project: args.project.clone() };

    if args.once {
        let snapshot = Snapshot::load(&store, &cfg, Utc::now())?;
        if ctx.json {
            let mut json = serde_json::to_value(&snapshot)?;
            json["project_filter"] = serde_json::json!(args.project);
            if let Some(project) = &args.project {
                json["tasks"]
                    .as_array_mut()
                    .expect("snapshot tasks are an array")
                    .retain(|t| t["execution_project"].as_str() == Some(project));
            }
            println!("{}", serde_json::to_string_pretty(&json)?);
        } else {
            let (width, height) = env.size.unwrap_or(dashboard::ONCE_DEFAULT_SIZE);
            print!("{}", dashboard::render_once(snapshot, options, width, height));
        }
        return Ok(0);
    }

    if let Err(e) = env.check() {
        print_error(&anyhow!(e));
        return Ok(EXIT_NO_TTY);
    }
    let paths = ctx.paths.clone();
    dashboard::run(&cfg, &paths, &store, options)?;
    Ok(0)
}
