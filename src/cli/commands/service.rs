//! `powerqueue service ...` — run the daemon under systemd (Linux) or launchd (macOS).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use owo_colors::{OwoColorize, Stream};

use crate::cli::output::print_warning;
use crate::cli::{Context, ServiceCommand, ServiceInstallArgs, ServiceLogsArgs, ServiceStopArgs};
use crate::secrets::SecretOrigin;
use crate::service::{
    InstalledUnit, Manager, ServiceSpec, ServiceState, parse_env_assignment, query, required_tools, unit_problems,
};

/// Handle `powerqueue service ...`.
pub fn run(ctx: &mut Context, cmd: ServiceCommand) -> Result<i32> {
    match cmd {
        ServiceCommand::Install(args) => install(ctx, args),
        ServiceCommand::Uninstall(args) => uninstall(ctx, args),
        ServiceCommand::Start => start(ctx),
        ServiceCommand::Stop(args) => stop_or_restart(ctx, args, false),
        ServiceCommand::Restart(args) => stop_or_restart(ctx, args, true),
        ServiceCommand::Status => status(ctx),
        ServiceCommand::Logs(args) => logs(ctx, args),
    }
}

fn ok_mark() -> String {
    "ok".if_supports_color(Stream::Stdout, |t| t.green()).to_string()
}

fn describe(manager: Manager) -> &'static str {
    match manager {
        Manager::Systemd => "systemd user unit",
        Manager::Launchd => "launchd agent",
    }
}

/// The pid of a live daemon that the service did not start, if any.
fn foreign_daemon(ctx: &mut Context, service_pid: Option<u32>) -> Option<u32> {
    if !ctx.paths.database().exists() {
        return None;
    }
    let tick_secs = ctx.config_or_default().ok()?.scheduler.tick_secs.max(1) as i64;
    let store = ctx.store().ok()?;
    if !store.daemon_alive(chrono::Duration::seconds(3 * tick_secs)).ok()? {
        return None;
    }
    let (pid, _) = store.daemon_heartbeat().ok()??;
    (Some(pid) != service_pid).then_some(pid)
}

/// Refuse to stop a running service whose unit would take tmux down with it.
fn guard_sessions(unit: Option<&InstalledUnit>, state: &ServiceState, force: bool, action: &str) -> Result<()> {
    if let Some(unit) = unit
        && !unit.keeps_sessions
        && state.running
        && !force
    {
        bail!(
            "{} has no `KillMode=process` / `AbandonProcessGroup`, so {action} would kill the tmux server and every \
             running session; regenerate it with `powerqueue service install --force` (it restarts the daemon with \
             the new settings and the sessions survive), or pass --force to {action} anyway",
            unit.path.display()
        );
    }
    Ok(())
}

/// After starting: give the daemon a moment, then report whether it stayed up.
fn settle(manager: Manager) -> Option<ServiceState> {
    std::thread::sleep(std::time::Duration::from_millis(1500));
    query(manager).ok()
}

fn install(ctx: &mut Context, args: ServiceInstallArgs) -> Result<i32> {
    let manager = match args.manager {
        Some(m) => m,
        None => Manager::detect()?,
    };
    let extra = args.env.iter().map(|e| parse_env_assignment(e)).collect::<Result<Vec<_>>>()?;
    let spec = ServiceSpec::from_environment(&ctx.paths, ctx.home.as_deref(), &extra)?;
    let text = spec.render(manager);
    if args.print {
        print!("{text}");
        return Ok(0);
    }
    if !ctx.is_initialised() {
        bail!(
            "no config at {}; run `powerqueue init` first (the service would fail to start without it)",
            ctx.paths.config_file().display()
        );
    }
    if args.linger && manager != Manager::Systemd {
        bail!("--linger is for systemd; launchd agents already run whenever you are logged in");
    }

    let path = manager.unit_path()?;
    let existing = InstalledUnit::load(manager)?;
    let changed = existing.as_ref().map(|u| u.text != text).unwrap_or(true);
    if let Some(old) = &existing
        && changed
        && !args.force
    {
        let diff = similar::TextDiff::from_lines(old.text.as_str(), text.as_str());
        eprintln!("{}", diff.unified_diff().header(&path.to_string_lossy(), "generated"));
        bail!("{} already exists and differs from the generated file (diff above); pass --force to replace it", path.display());
    }

    // Probe the manager before touching anything, so a missing user bus fails early.
    let before = query(manager).with_context(|| format!("ask {manager} about the powerqueue service"))?;
    if changed {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        std::fs::write(&path, &text).with_context(|| format!("write {}", path.display()))?;
        tracing::info!(path = %path.display(), %manager, "service unit written");
    }
    // launchd writes the daemon's output under the logs directory.
    ctx.paths.ensure()?;

    let foreign = foreign_daemon(ctx, before.pid);
    let action = if before.running {
        if changed {
            crate::service::reload_and_restart(manager)?;
            "restarted to apply the new unit"
        } else {
            crate::service::activate(manager, false)?;
            "already running"
        }
    } else if args.no_start {
        crate::service::activate(manager, false)?;
        "enabled, not started (--no-start)"
    } else if foreign.is_some() {
        crate::service::activate(manager, false)?;
        "enabled, not started: a daemon is already running outside the service"
    } else {
        crate::service::activate(manager, true)?;
        "started"
    };
    if args.linger {
        crate::service::enable_linger()?;
    }
    let after = if action == "started" || action.starts_with("restarted") { settle(manager) } else { query(manager).ok() };

    let tools = ctx.config_or_default().map(required_tools).unwrap_or_default();
    let installed = InstalledUnit::parse(manager, path.clone(), text);
    let missing = installed.missing_tools(&tools);
    let env_secrets: Vec<&'static str> = crate::cli::commands::secrets::secret_rows(ctx.secrets())
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.origin == Some(SecretOrigin::Environment) && spec.env_value(r.env_var).is_none())
        .map(|r| r.env_var)
        .collect();

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "manager": manager,
                "path": path,
                "changed": changed,
                "action": action,
                "spec": spec,
                "state": after,
                "foreign_daemon_pid": foreign,
                "missing_tools": missing,
                "secrets_only_in_environment": env_secrets,
            }))?
        );
        return Ok(0);
    }

    let verb = if changed { "wrote" } else { "unchanged" };
    println!("{} {verb} {} ({})", ok_mark(), path.display(), describe(manager));
    println!("   runs {} run", spec.binary.display());
    println!("   PATH {}", spec.env_value("PATH").unwrap_or("-"));
    match &after {
        Some(s) if s.running => println!("   {action} (pid {})", s.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())),
        Some(s) if action == "started" || action.starts_with("restarted") => {
            print_warning(&format!("the service is not running ({}); see `powerqueue service logs`", s.detail))
        }
        _ => println!("   {action}"),
    }
    if let Some(pid) = foreign {
        println!("   a daemon (pid {pid}) is running outside the service: `powerqueue stop`, then `powerqueue service start`");
    }
    if !missing.is_empty() {
        print_warning(&format!(
            "the service's PATH cannot find {}; re-run from a shell where they are on PATH or add `--env PATH=...`",
            missing.join(", ")
        ));
    }
    for var in &env_secrets {
        print_warning(&format!(
            "{var} is only in your shell's environment, which the service does not see; \
             store it with `powerqueue secrets set` instead"
        ));
    }
    if looks_like_build_output(&spec.binary) {
        print_warning(&format!(
            "{} is a cargo build output; `cargo clean` or a rebuild breaks the service. Install the binary \
             (`cargo install --path .` or the release installer) and re-run this command from it",
            spec.binary.display()
        ));
    }
    if manager == Manager::Systemd && !args.linger && after.as_ref().and_then(|s| s.linger) == Some(false) {
        println!(
            "   note: the service stops when you log out; on a server, `powerqueue service install --linger` keeps it \
             running and starts it at boot"
        );
    }
    println!("   check on it: `powerqueue service status`, `powerqueue service logs -f`, `powerqueue dashboard`");
    Ok(0)
}

/// `.../target/{debug,release}/powerqueue`.
fn looks_like_build_output(binary: &Path) -> bool {
    let parts: Vec<_> = binary.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    parts.windows(2).any(|w| w[0] == "target" && (w[1] == "debug" || w[1] == "release"))
}

fn uninstall(ctx: &mut Context, args: ServiceStopArgs) -> Result<i32> {
    let manager = Manager::detect()?;
    let Some(unit) = InstalledUnit::load(manager)? else {
        let path = manager.unit_path()?;
        if ctx.json {
            println!("{}", serde_json::json!({ "manager": manager, "path": path, "removed": false }));
        } else {
            println!("no {} at {}; nothing to remove", describe(manager), path.display());
        }
        return Ok(0);
    };
    let state = query(manager)?;
    guard_sessions(Some(&unit), &state, args.force, "uninstalling")?;
    if let Err(e) = crate::service::deactivate(manager) {
        print_warning(&format!("could not stop/disable the service: {e:#}"));
    }
    std::fs::remove_file(&unit.path).with_context(|| format!("remove {}", unit.path.display()))?;
    crate::service::forget(manager)?;
    tracing::info!(path = %unit.path.display(), %manager, "service unit removed");
    if ctx.json {
        println!("{}", serde_json::json!({ "manager": manager, "path": unit.path, "removed": true }));
    } else {
        println!("{} removed {} ({})", ok_mark(), unit.path.display(), describe(manager));
        if state.running {
            println!("   the daemon was stopped; tmux sessions keep running (`powerqueue run` to supervise them again)");
        }
    }
    Ok(0)
}

fn require_unit(manager: Manager) -> Result<InstalledUnit> {
    InstalledUnit::load(manager)?.with_context(|| {
        format!(
            "no {} at {}; install it with `powerqueue service install`",
            describe(manager),
            manager.unit_path().map(|p| p.display().to_string()).unwrap_or_default()
        )
    })
}

fn start(ctx: &mut Context) -> Result<i32> {
    let manager = Manager::detect()?;
    require_unit(manager)?;
    let state = query(manager)?;
    if state.running {
        println!("already running (pid {})", state.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()));
        return Ok(0);
    }
    if let Some(pid) = foreign_daemon(ctx, None) {
        bail!("a daemon (pid {pid}) is already running outside the service; `powerqueue stop` it first");
    }
    crate::service::start(manager)?;
    report_started(ctx, manager, "started")
}

fn stop_or_restart(ctx: &mut Context, args: ServiceStopArgs, restart: bool) -> Result<i32> {
    let manager = Manager::detect()?;
    let unit = require_unit(manager)?;
    let state = query(manager)?;
    let action = if restart { "restarting" } else { "stopping" };
    guard_sessions(Some(&unit), &state, args.force, action)?;
    if restart {
        if !state.running
            && let Some(pid) = foreign_daemon(ctx, None)
        {
            bail!("a daemon (pid {pid}) is running outside the service; `powerqueue stop` it first");
        }
        crate::service::restart(manager)?;
        return report_started(ctx, manager, "restarted");
    }
    if !state.running {
        println!("not running ({})", state.detail);
        return Ok(0);
    }
    crate::service::stop(manager)?;
    if ctx.json {
        println!("{}", serde_json::json!({ "manager": manager, "stopped": true }));
    } else {
        println!(
            "{} stop requested; the daemon exits after its current tick (sessions keep running in tmux; \
             `powerqueue service start` to bring it back)",
            ok_mark()
        );
    }
    Ok(0)
}

fn report_started(ctx: &Context, manager: Manager, what: &str) -> Result<i32> {
    let state = settle(manager);
    let running = state.as_ref().is_some_and(|s| s.running);
    if ctx.json {
        println!("{}", serde_json::json!({ "manager": manager, "action": what, "state": state }));
    } else if running {
        let pid = state.and_then(|s| s.pid).map(|p| p.to_string()).unwrap_or_else(|| "?".into());
        println!("{} {what} (pid {pid})", ok_mark());
    } else {
        let detail = state.map(|s| s.detail).unwrap_or_else(|| "unknown".into());
        print_warning(&format!("the service is not running ({detail}); see `powerqueue service logs`"));
        return Ok(1);
    }
    Ok(0)
}

fn status(ctx: &mut Context) -> Result<i32> {
    let manager = Manager::detect()?;
    let path = manager.unit_path()?;
    let unit = InstalledUnit::load(manager)?;
    let state = query(manager);
    let tools = ctx.config_or_default().map(required_tools).unwrap_or_default();
    let exe: Option<PathBuf> = std::env::current_exe().ok();
    let problems = unit.as_ref().map(|u| unit_problems(u, exe.as_deref(), &tools)).unwrap_or_default();
    let running = state.as_ref().is_ok_and(|s| s.running);
    let foreign = foreign_daemon(ctx, state.as_ref().ok().and_then(|s| s.pid));

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "manager": manager,
                "path": path,
                "installed": unit.is_some(),
                "unit": unit,
                "state": state.as_ref().ok(),
                "state_error": state.as_ref().err().map(|e| format!("{e:#}")),
                "problems": problems,
                "foreign_daemon_pid": foreign,
            }))?
        );
        return Ok(if running { 0 } else { 3 });
    }

    let yes_no = |b: bool| if b { "yes" } else { "no" };
    let Some(unit) = unit else {
        println!("{:<9}not installed ({})", "service", path.display());
        println!("{:<9}`powerqueue service install` keeps the daemon running and restarts it after a crash", "");
        if let Some(pid) = foreign {
            println!("{:<9}a daemon is running outside any service (pid {pid})", "daemon");
        }
        return Ok(3);
    };
    let origin = if unit.generated { "generated by powerqueue" } else { "hand-written" };
    println!("{:<9}{} ({}, {origin})", "unit", unit.path.display(), describe(manager));
    println!("{:<9}{}", "binary", unit.binary.as_ref().map(|b| b.display().to_string()).unwrap_or_else(|| "-".into()));
    match &state {
        Ok(s) => {
            println!("{:<9}{}", "enabled", yes_no(s.enabled));
            let run = if s.running {
                format!("yes (pid {}, {})", s.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()), s.detail)
            } else {
                format!("no ({})", s.detail)
            };
            println!("{:<9}{run}", "running");
            if let Some(linger) = s.linger {
                let text =
                    if linger { "on (runs without a login session, starts at boot)" } else { "off (stops when you log out)" };
                println!("{:<9}{text}", "linger");
            }
        }
        Err(e) => println!("{:<9}unknown: {e:#}", "running"),
    }
    let sessions =
        if unit.keeps_sessions { "survive a stop/restart of the service" } else { "are killed when the service stops" };
    println!("{:<9}{sessions}", "sessions");
    if let Some(pid) = foreign {
        println!("{:<9}a daemon is running outside the service (pid {pid})", "daemon");
    }
    for p in &problems {
        print_warning(&format!("{}; {}", p.detail, p.hint));
    }
    if !running {
        println!("start it with `powerqueue service start`; `powerqueue service logs` shows why it stopped");
    }
    Ok(if running { 0 } else { 3 })
}

fn logs(ctx: &mut Context, args: ServiceLogsArgs) -> Result<i32> {
    let manager = Manager::detect()?;
    let mut cmd = manager.logs_command(&ctx.paths, args.lines, args.follow);
    let program = cmd.get_program().to_string_lossy().into_owned();
    let status = cmd.status().with_context(|| format!("run `{program}`"))?;
    if !ctx.json && !args.follow {
        eprintln!("(the daemon's own log, with task details: `powerqueue logs`)");
    }
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_outputs_are_spotted() {
        assert!(looks_like_build_output(Path::new("/home/me/powerqueue/target/release/powerqueue")));
        assert!(looks_like_build_output(Path::new("/w/target/debug/powerqueue")));
        assert!(!looks_like_build_output(Path::new("/home/me/.cargo/bin/powerqueue")));
        assert!(!looks_like_build_output(Path::new("/srv/target/powerqueue")));
    }
}
