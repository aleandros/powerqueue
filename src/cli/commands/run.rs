//! `powerqueue run` / `powerqueue stop` / `powerqueue pause` / `powerqueue resume`.

use anyhow::{Context as _, Result};
use owo_colors::{OwoColorize, Stream};

use crate::cli::commands::runtime;
use crate::cli::output::ago;
use crate::cli::{Context, PauseArgs, RunArgs};
use crate::domain::{DaemonCommand, SCHEDULING_PAUSE_KEY, SchedulingPause};
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
        let providers: Vec<String> = cfg.budget.enabled_providers_in_order().iter().map(|p| p.to_string()).collect();
        let probes = match cfg.budget.probe_interval_mins {
            0 => "usage probes off".to_string(),
            m => format!("usage probes every {m}m"),
        };
        eprintln!(
            "providers {} (in fallback order) | {probes}",
            if providers.is_empty() { "none enabled".to_string() } else { providers.join(", ") }
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

/// `powerqueue pause`: stop launching sessions. A live daemon is asked to
/// record the pause (so the event lands in its log); without one the pause
/// is written directly and the next daemon honours it. Idempotent.
pub fn pause(ctx: &mut Context, args: PauseArgs) -> Result<i32> {
    let tick_secs = ctx.config_or_default()?.scheduler.tick_secs.max(1) as i64;
    let store = ctx.store()?;
    let now = chrono::Utc::now();
    let reason = args.reason.filter(|r| !r.trim().is_empty());
    let already = SchedulingPause::load(store)?;
    let was_paused = already.is_some();
    let alive = store.daemon_alive(chrono::Duration::seconds(3 * tick_secs))?;
    // The kv row is the switch the daemon reads every tick, so write it
    // here: it takes effect (and `resume`/`status` see it) at once. A live
    // daemon is also told, so the event lands in its log.
    let pause = match already {
        Some(p) => p,
        None => {
            let p = SchedulingPause { since: now, reason: reason.clone() };
            store.kv_set(SCHEDULING_PAUSE_KEY, &p).context("record the pause")?;
            if alive {
                store.enqueue_command(&DaemonCommand::PauseScheduling { reason }).context("queue pause command")?;
            }
            p
        }
    };
    let live = store.list_live_sessions()?.len();
    if ctx.json {
        println!(
            "{}",
            serde_json::json!({ "paused": true, "since": pause.since, "reason": pause.reason, "daemon_running": alive, "live_sessions": live })
        );
        return Ok(0);
    }
    if was_paused {
        println!("scheduling is already {}", pause.describe());
    } else if alive {
        println!(
            "scheduling paused: no new session will start until `powerqueue resume`; {live} running session(s) continue and the daemon keeps monitoring them"
        );
    } else {
        println!("scheduling paused (no daemon running; the next `powerqueue run` starts paused until `powerqueue resume`)");
    }
    Ok(0)
}

/// `powerqueue resume`: launch sessions again. Idempotent.
pub fn resume(ctx: &mut Context) -> Result<i32> {
    let tick_secs = ctx.config_or_default()?.scheduler.tick_secs.max(1) as i64;
    let store = ctx.store()?;
    let was = SchedulingPause::load(store)?;
    let alive = store.daemon_alive(chrono::Duration::seconds(3 * tick_secs))?;
    if was.is_some() {
        store.kv_delete(SCHEDULING_PAUSE_KEY).context("clear the pause")?;
        if alive {
            store.enqueue_command(&DaemonCommand::ResumeScheduling).context("queue resume command")?;
        }
    }
    if ctx.json {
        println!("{}", serde_json::json!({ "paused": false, "was_paused": was.is_some(), "daemon_running": alive }));
        return Ok(0);
    }
    match (was, alive) {
        (None, _) => println!("scheduling was not paused"),
        (Some(p), true) => println!("scheduling resumes on the daemon's next tick (was {})", p.describe()),
        (Some(p), false) => println!("pause cleared (was {}); no daemon running, start one with `powerqueue run`", p.describe()),
    }
    Ok(0)
}
