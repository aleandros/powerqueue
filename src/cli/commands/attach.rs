//! `powerqueue attach`.
//!
//! With a task: select that task's tmux window and attach (or `switch-client`
//! when already inside tmux). Without one: attach to the hosting session.
//! `--print` shows the tmux command line instead of running it.

use anyhow::{Result, anyhow, bail};

use crate::cli::{AttachArgs, Context};
use crate::tmux::{Tmux, shell_quote};

pub fn run(ctx: &mut Context, args: AttachArgs) -> Result<i32> {
    let cfg = ctx.config_cloned()?;
    let tmux = Tmux::new(cfg.tmux.binary.clone(), cfg.tmux.socket_name.clone());
    let session_name = cfg.tmux.session_name.clone();

    let window = match args.task.as_deref() {
        Some(needle) => Some(resolve_window(ctx, needle)?),
        None => None,
    };

    if args.print {
        println!("{}", print_command(&tmux, &session_name, window.as_deref()));
        return Ok(0);
    }

    if !tmux.has_session(&session_name)? {
        bail!("tmux session `{session_name}` does not exist; start the daemon with `powerqueue run` first");
    }
    if let Some(w) = window.as_deref() {
        let exists = tmux.list_panes(&session_name)?.iter().any(|p| p.window_id == w);
        if !exists {
            bail!(
                "the tmux window {w} for this task is gone (closed or cleaned up); \
                 `powerqueue task show {}` has the timeline, `powerqueue task retry` starts it again",
                args.task.as_deref().unwrap_or_default()
            );
        }
    }
    tracing::info!(session = %session_name, window = window.as_deref().unwrap_or("-"), "attaching");
    // On unix this replaces the process and never returns on success.
    tmux.attach(&session_name, window.as_deref())?;
    Ok(0)
}

/// Resolve a task reference to the tmux window id of its latest session.
fn resolve_window(ctx: &mut Context, needle: &str) -> Result<String> {
    let store = ctx.store()?;
    let task = store.find_task(needle)?.ok_or_else(|| anyhow!("no task matches `{needle}` (see `powerqueue status --all`)"))?;
    let session = store.latest_session(task.id)?.ok_or_else(|| {
        anyhow!(
            "task {} ({}) has no session yet (state: {}); the daemon starts one when it is scheduled (`powerqueue run`)",
            task.key,
            task.id.short(),
            task.state
        )
    })?;
    if session.tmux_window.trim().is_empty() {
        bail!("task {} has a session record without a tmux window; was it launched by an older version?", task.key);
    }
    if !session.state.is_live() {
        tracing::warn!(task = %task.key, session = %session.id, state = %session.state, "latest session is not live; attaching anyway");
    }
    Ok(session.tmux_window)
}

/// The shell line equivalent to what [`Tmux::attach`] does.
fn print_command(tmux: &Tmux, session: &str, window: Option<&str>) -> String {
    let prefix = tmux.command_prefix().iter().map(|p| shell_quote(p)).collect::<Vec<_>>().join(" ");
    let attach = tmux.attach_args(session, window).iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    match window {
        Some(w) => format!("{prefix} select-window -t {} && {prefix} {attach}", shell_quote(&format!("{session}:{w}"))),
        None => format!("{prefix} {attach}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_command_shows_socket_and_window() {
        let t = Tmux::new("tmux", Some("pq-sock".into()));
        let line = print_command(&t, "powerqueue", Some("@3"));
        assert!(line.starts_with("tmux -L pq-sock select-window -t powerqueue:@3 && tmux -L pq-sock "), "{line}");
        assert!(line.contains("-t powerqueue"));
        let line = print_command(&Tmux::new("tmux", None), "powerqueue", None);
        assert!(line == "tmux attach-session -t powerqueue" || line == "tmux switch-client -t powerqueue", "{line}");
    }
}
