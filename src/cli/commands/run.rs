//! `powerqueue run` / `powerqueue stop`.

use anyhow::{Context as _, Result};
use owo_colors::{OwoColorize, Stream};

use crate::cli::commands::runtime;
use crate::cli::output::ago;
use crate::cli::{Context, RunArgs};
use crate::domain::DaemonCommand;
use crate::scheduler::Daemon;
use crate::secrets::Secrets;

/// Start the scheduler in the foreground (one tick with `--once`).
pub fn run(ctx: &mut Context, args: RunArgs) -> Result<i32> {
    let cfg = ctx.config_cloned()?;
    cfg.ensure_valid()?;
    let store = ctx.store()?.clone();
    let paths = ctx.paths.clone();
    let secrets = Secrets::auto(&paths);

    if !ctx.json {
        let team = if cfg.linear.team_keys.is_empty() { "all teams".to_string() } else { cfg.linear.team_keys.join(", ") };
        let linear = if args.offline || !cfg.linear.enabled { "offline".to_string() } else { team };
        eprintln!(
            "{} repo {} | linear {} | max {} concurrent | tmux session {} | log {}",
            "powerqueue".if_supports_color(Stream::Stdout, |t| t.bold()),
            cfg.repo.path,
            linear,
            cfg.scheduler.max_concurrent,
            cfg.tmux.session_name,
            paths.log_file().display()
        );
        if args.once {
            eprintln!("running a single scheduling pass");
        } else {
            eprintln!("press ctrl-c or run `powerqueue stop` to exit; sessions keep running in tmux");
        }
    }

    let mut daemon = Daemon::new(cfg, paths, store, secrets)?;
    daemon.offline = args.offline;
    runtime()?.block_on(daemon.run(args.once))?;
    Ok(0)
}

/// Ask a running daemon to exit after its current tick.
pub fn stop(ctx: &mut Context) -> Result<i32> {
    let tick_secs = ctx.config_or_default()?.scheduler.tick_secs.max(1) as i64;
    let store = ctx.store()?;
    store.enqueue_command(&DaemonCommand::Shutdown).context("queue shutdown command")?;
    let alive = store.daemon_alive(chrono::Duration::seconds(3 * tick_secs))?;
    if !alive {
        let last = match store.daemon_heartbeat()? {
            Some((pid, at)) => format!("last heartbeat {} from pid {pid}", ago(at)),
            None => "no heartbeat recorded".to_string(),
        };
        if ctx.json {
            println!("{}", serde_json::json!({ "running": false, "last_heartbeat": last }));
        } else {
            eprintln!("no running daemon detected ({last}); the shutdown request will apply to the next daemon that starts");
        }
        return Ok(1);
    }
    if ctx.json {
        println!("{}", serde_json::json!({ "running": true, "requested": "shutdown" }));
    } else {
        println!("shutdown requested; the daemon exits after its current tick (sessions keep running in tmux)");
    }
    Ok(0)
}
