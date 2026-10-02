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

pub use app::{Action, DashboardApp, RowData, Snapshot, rows_for};

use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::cli::commands::add::{NewTask, create_task};
use crate::cli::commands::task::apply_offline;
use crate::config::Config;
use crate::domain::DaemonCommand;
use crate::paths::Paths;
use crate::store::Store;
use crate::tmux::Tmux;

/// How often the snapshot is reloaded from the database.
pub const REFRESH_EVERY: Duration = Duration::from_secs(2);
/// How long one event poll waits before the loop checks the refresh timer.
pub const POLL_EVERY: Duration = Duration::from_millis(250);

/// What the event loop does after applying an action.
enum Flow {
    Continue,
    Quit,
}

/// Run the dashboard until the user quits.
pub fn run(cfg: &Config, paths: &Paths, store: &Store) -> Result<()> {
    let _ = paths;
    let mut app = DashboardApp::new(Snapshot::load(store, cfg, Utc::now())?);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        previous_hook(info);
    }));
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, cfg, store);
    ratatui::restore();
    let _ = std::panic::take_hook();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut DashboardApp, cfg: &Config, store: &Store) -> Result<()> {
    let mut last_refresh = Instant::now();
    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(POLL_EVERY)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(action) = app.handle_key(key) {
                        match apply(action, app, cfg, store, terminal)? {
                            Flow::Quit => return Ok(()),
                            Flow::Continue => last_refresh = Instant::now() - REFRESH_EVERY,
                        }
                    }
                }
                _ => {}
            }
        }
        if last_refresh.elapsed() >= REFRESH_EVERY {
            match Snapshot::load(store, cfg, Utc::now()) {
                Ok(s) => app.set_snapshot(s),
                Err(e) => app.status_line = Some(format!("refresh failed: {e:#}")),
            }
            last_refresh = Instant::now();
        }
    }
}

fn control(app: &mut DashboardApp, store: &Store, cmd: DaemonCommand, verb: &str) -> Result<()> {
    let task_id = match &cmd {
        DaemonCommand::Pause { task_id }
        | DaemonCommand::Resume { task_id }
        | DaemonCommand::Cancel { task_id }
        | DaemonCommand::Retry { task_id } => *task_id,
        _ => return Ok(()),
    };
    let Some(mut task) = store.get_task(task_id)? else {
        app.status_line = Some("task no longer exists".into());
        return Ok(());
    };
    store.enqueue_command(&cmd)?;
    if app.snapshot.daemon_alive {
        app.status_line = Some(format!("{verb} {} queued for the daemon", task.key));
    } else if apply_offline(store, &mut task, &cmd)? {
        app.status_line = Some(format!("{verb} {} applied directly (daemon not running)", task.key));
    } else {
        app.status_line = Some(format!("{verb} {} queued; cannot apply offline from state {}", task.key, task.state));
    }
    Ok(())
}

fn apply(
    action: Action,
    app: &mut DashboardApp,
    cfg: &Config,
    store: &Store,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<Flow> {
    match action {
        Action::Quit => return Ok(Flow::Quit),
        Action::Pause(id) => control(app, store, DaemonCommand::Pause { task_id: id }, "pause")?,
        Action::Resume(id) => control(app, store, DaemonCommand::Resume { task_id: id }, "resume")?,
        Action::Cancel(id) => control(app, store, DaemonCommand::Cancel { task_id: id }, "cancel")?,
        Action::Retry(id) => control(app, store, DaemonCommand::Retry { task_id: id }, "retry")?,
        Action::Attach(id) => {
            let session = app.snapshot.latest_session(id).cloned();
            match session {
                None => app.status_line = Some("no session to attach to yet".into()),
                Some(s) => {
                    let tmux = Tmux::new(cfg.tmux.binary.clone(), cfg.tmux.socket_name.clone());
                    ratatui::restore();
                    // On unix this replaces the process and never returns.
                    let outcome = tmux.attach(&s.tmux_session, Some(&s.tmux_window));
                    *terminal = ratatui::init();
                    if let Err(e) = outcome {
                        app.status_line = Some(format!("attach failed: {e:#}"));
                    }
                }
            }
        }
        Action::Refresh | Action::ToggleTerminal | Action::Help | Action::OpenAdd => {}
        Action::AddTask(title) => match create_task(store, &NewTask { title, ..Default::default() }) {
            Ok(t) => app.status_line = Some(format!("added {} ({})", t.key, t.id.short())),
            Err(e) => app.status_line = Some(format!("add failed: {e:#}")),
        },
    }
    Ok(Flow::Continue)
}
