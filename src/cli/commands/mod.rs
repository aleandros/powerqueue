//! Command handlers. Each file owns one top-level command; `dispatch` is the
//! only place that knows about all of them.

pub mod add;
pub mod attach;
pub mod budget;
pub mod config;
pub mod doctor;
pub mod hook;
pub mod init;
pub mod linear;
pub mod logs;
pub mod priority;
pub mod run;
pub mod secrets;
pub mod status;
pub mod task;

use anyhow::Result;
use clap::CommandFactory;

use crate::cli::{Cli, Command, Context};
use crate::logging::Verbosity;

/// Route a parsed CLI to its handler. Returns the exit code.
pub fn dispatch(cli: Cli) -> Result<i32> {
    let verbosity = Verbosity::from_flags(cli.verbose, cli.quiet);
    let no_color_env = std::env::var_os("NO_COLOR").map(|v| !v.is_empty()).unwrap_or(false);
    let color = !cli.no_color && !no_color_env && std::io::IsTerminal::is_terminal(&std::io::stdout());
    crate::cli::output::set_color(color);
    let mut ctx = Context::new(cli.home.clone(), verbosity, cli.json, color);

    match cli.command {
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "powerqueue", &mut std::io::stdout());
            Ok(0)
        }
        Command::Hook(args) => hook::run(&mut ctx, args),
        Command::Init(args) => init::run(&mut ctx, args),
        Command::Dashboard => {
            ctx.init_logging(false)?;
            crate::dashboard::run(&ctx.config_cloned()?, &ctx.paths.clone(), &ctx.store()?.clone())?;
            Ok(0)
        }
        other => {
            ctx.init_logging(true)?;
            match other {
                Command::Run(args) => run::run(&mut ctx, args),
                Command::Stop => run::stop(&mut ctx),
                Command::Status(args) => status::run(&mut ctx, args),
                Command::Add(args) => add::run(&mut ctx, args),
                Command::Task(cmd) => task::run(&mut ctx, cmd),
                Command::Attach(args) => attach::run(&mut ctx, args),
                Command::Priority(cmd) => priority::run(&mut ctx, cmd),
                Command::Budget(cmd) => budget::run(&mut ctx, cmd),
                Command::Linear(cmd) => linear::run(&mut ctx, cmd),
                Command::Doctor(args) => doctor::run(&mut ctx, args),
                Command::Logs(args) => logs::run(&mut ctx, args),
                Command::Config(cmd) => config::run(&mut ctx, cmd),
                Command::Secrets(cmd) => secrets::run(&mut ctx, cmd),
                Command::Completions { .. } | Command::Hook(_) | Command::Init(_) | Command::Dashboard => unreachable!(),
            }
        }
    }
}

/// Build a tokio runtime for async handlers.
pub fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}
