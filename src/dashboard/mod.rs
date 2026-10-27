//! Live terminal dashboard (ratatui).
//!
//! Read-only against the database except for the few commands it offers
//! (pause/resume/cancel/retry/add/attach), which go through the `commands`
//! table like the CLI does. Layout:
//!
//! ```text
//! ┌ powerqueue ─ daemon ● running ─ 2/2 slots ─ period 43% (day 3.1/7) ────┐
//! │ tasks table: key | state | crit | model | tokens | cpu/rss | age | title │
//! │ budget gauges per provider │ selected task: timeline / last output       │
//! │ footer: keys                                                              │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! [`TerminalEnv`] holds the preconditions (TTY, `TERM`, locale) so the CLI
//! can refuse to start the TUI with a clear message; [`render_once`] draws one
//! frame into an in-memory buffer for `dashboard --once` and bug reports.

pub mod app;
pub mod keys;
pub mod ui;

pub use app::{Action, DashboardApp, RowData, Snapshot, rows_for};

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use chrono::Utc;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::cli::commands::add::{NewTask, create_task_in_config};
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
/// Frame size for `--once` when stdout is not a terminal.
pub const ONCE_DEFAULT_SIZE: (u16, u16) = (120, 36);

/// Rendering options chosen by the CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Restrict displayed tasks; shared ledgers and slot counts remain unfiltered.
    pub project: Option<String>,
    /// Draw with ASCII symbols and `+-|` borders instead of Unicode glyphs.
    pub ascii: bool,
}

/// Why the live dashboard cannot start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TerminalError {
    #[error(
        "the dashboard needs an interactive terminal (stdin/stdout are not a TTY); \
         use `powerqueue status` or run it from a terminal window"
    )]
    NotATty,
    #[error(
        "the dashboard needs an interactive terminal (TERM is {0}); \
         use `powerqueue status` or run it from a terminal window"
    )]
    BadTerm(String),
}

/// Facts about the terminal the dashboard was started from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalEnv {
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    /// `$TERM`, when set.
    pub term: Option<String>,
    /// Whether `LC_ALL`/`LC_CTYPE`/`LANG` select a UTF-8 charset.
    pub utf8_locale: bool,
    /// Terminal size when stdout is a terminal.
    pub size: Option<(u16, u16)>,
}

impl TerminalEnv {
    /// Inspect stdin/stdout, `TERM` and the locale variables.
    pub fn detect() -> Self {
        let stdout_tty = std::io::stdout().is_terminal();
        let size = if stdout_tty { ratatui::crossterm::terminal::size().ok() } else { None };
        Self {
            stdin_tty: std::io::stdin().is_terminal(),
            stdout_tty,
            term: std::env::var("TERM").ok(),
            utf8_locale: locale_is_utf8(
                std::env::var("LC_ALL").ok().as_deref(),
                std::env::var("LC_CTYPE").ok().as_deref(),
                std::env::var("LANG").ok().as_deref(),
            ),
            size,
        }
    }

    /// The preconditions for the live TUI, as a pure function of the facts.
    pub fn check(&self) -> Result<(), TerminalError> {
        precheck(self.stdin_tty, self.stdout_tty, self.term.as_deref())
    }
}

/// Decide whether the live TUI may start. `term` is `$TERM`.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "Keep the existing precheck flag interface during lint adoption; callers already supply named state predicates."
)]
pub fn precheck(stdin_tty: bool, stdout_tty: bool, term: Option<&str>) -> Result<(), TerminalError> {
    if !stdin_tty || !stdout_tty {
        return Err(TerminalError::NotATty);
    }
    match term.map(str::trim) {
        None | Some("") => Err(TerminalError::BadTerm("not set".into())),
        Some(t) if t.eq_ignore_ascii_case("dumb") => Err(TerminalError::BadTerm("`dumb`".into())),
        Some(_) => Ok(()),
    }
}

/// Whether the effective locale (`LC_ALL` > `LC_CTYPE` > `LANG`) is UTF-8.
/// With none of them set the terminal is assumed to be UTF-8, which is what
/// every modern emulator defaults to.
pub fn locale_is_utf8(lc_all: Option<&str>, lc_ctype: Option<&str>, lang: Option<&str>) -> bool {
    let effective = [lc_all, lc_ctype, lang].into_iter().flatten().map(str::trim).find(|v| !v.is_empty());
    match effective {
        None => true,
        Some(v) => {
            let v = v.to_ascii_lowercase();
            v.contains("utf-8") || v.contains("utf8")
        }
    }
}

/// What the event loop does after applying an action.
enum Flow {
    Continue,
    Quit,
}

/// Run the dashboard until the user quits. Fails when the terminal cannot be
/// put into raw mode; call [`TerminalEnv::check`] first for a friendlier message.
pub fn run(cfg: &Config, paths: &Paths, store: &Store, options: Options) -> Result<()> {
    let mut app = DashboardApp::new(Snapshot::load(store, cfg, Utc::now())?);
    app.ascii = options.ascii;
    app.project_filter = options.project;
    // ratatui installs its own restoring hook in `try_init`; ours makes sure
    // the terminal is sane even when a panic happens before/after that.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_and_clear_terminal();
        previous_hook(info);
    }));
    let mut terminal = ratatui::try_init().context("initialise terminal (raw mode + alternate screen)")?;
    // The first frame only writes cells that differ from an empty buffer, so
    // whatever the terminal showed before (shell history, a tmux pane that
    // ignores the alternate screen) would stay visible behind the dashboard.
    // Clearing forces a full redraw of the very first frame.
    let result = terminal.clear().context("clear terminal").and_then(|()| event_loop(&mut terminal, &mut app, cfg, paths, store));
    restore_and_clear_terminal();
    let _ = std::panic::take_hook();
    result
}

/// Clear both the dashboard buffer and the restored shell screen. Clearing
/// before leaving also works in tmux panes with alternate-screen disabled.
/// Best effort so cleanup cannot hide the original error or panic.
fn restore_and_clear_terminal() {
    use ratatui::crossterm::{
        cursor::MoveTo,
        execute,
        style::ResetColor,
        terminal::{Clear, ClearType},
    };
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, ResetColor, Clear(ClearType::All), MoveTo(0, 0));
    ratatui::restore();
    let _ = execute!(stdout, ResetColor, Clear(ClearType::All), MoveTo(0, 0));
}

/// Draw one frame of `snapshot` into a `width`×`height` buffer and return it
/// as plain text (trailing spaces trimmed, one line per row).
pub fn render_once(snapshot: Snapshot, options: Options, width: u16, height: u16) -> String {
    let mut app = DashboardApp::new(snapshot);
    app.ascii = options.ascii;
    app.project_filter = options.project;
    render_once_app(&app, width, height)
}

/// [`render_once`] for an already built app (keeps its `ascii` flag and state).
pub fn render_once_app(app: &DashboardApp, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width.max(20), height.max(6));
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => return format!("cannot create frame buffer: {e}\n"),
    };
    if let Err(e) = terminal.draw(|f| ui::draw(f, app)) {
        return format!("cannot draw frame: {e}\n");
    }
    buffer_to_text(terminal.backend().buffer())
}

/// Rows of a buffer as text, trailing whitespace removed.
pub fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
    let width = usize::from(buffer.area.width);
    let mut out = String::new();
    for row in buffer.content().chunks(width.max(1)) {
        let line: String = row.iter().map(ratatui::buffer::Cell::symbol).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut DashboardApp,
    cfg: &Config,
    paths: &Paths,
    store: &Store,
) -> Result<()> {
    let mut last_refresh = Instant::now();
    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(POLL_EVERY)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(action) = app.handle_key(key) {
                        match apply(action, app, cfg, paths, store, terminal)? {
                            Flow::Quit => return Ok(()),
                            Flow::Continue => {
                                last_refresh = Instant::now()
                                    .checked_sub(REFRESH_EVERY)
                                    .expect("monotonic clock supports the refresh interval");
                            }
                        }
                    }
                }
                // A resize leaves stale cells outside the new layout; redraw from scratch.
                Event::Resize(_, _) => terminal.clear()?,
                _ => {}
            }
        }
        if last_refresh.elapsed() >= REFRESH_EVERY {
            match Snapshot::load(store, cfg, Utc::now()) {
                Ok(s) => {
                    if s.has_new_attention_since(&app.snapshot) {
                        // The terminal decides whether BEL is audible, visual,
                        // or ignored. No external notification service needed.
                        let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::style::Print('\u{7}'));
                    }
                    app.set_snapshot(s);
                }
                Err(e) => app.status_line = Some(format!("refresh failed: {e:#}")),
            }
            last_refresh = Instant::now();
        }
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "Keep the established ownership-taking boundary for callers and serialized inputs."
)]
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

/// Jump to a task's tmux window. Inside the powerqueue tmux server this is a
/// `switch-client` and the dashboard keeps running in its own window; from
/// anywhere else the process is replaced by `tmux attach` (nested attaches
/// get `$TMUX` cleared so tmux does not refuse them).
fn attach(app: &mut DashboardApp, cfg: &Config, terminal: &mut ratatui::DefaultTerminal, session: &crate::domain::Session) {
    let tmux = Tmux::new(cfg.tmux.binary.clone(), cfg.tmux.socket_name.clone());
    if tmux.inside_this_server() {
        match tmux.switch_client(&session.tmux_session, Some(&session.tmux_window)) {
            Ok(()) => app.status_line = Some(format!("switched to window {}", session.tmux_window)),
            Err(e) => app.status_line = Some(format!("switch failed: {e:#}")),
        }
        return;
    }
    restore_and_clear_terminal();
    // On unix this replaces the process and never returns on success.
    let outcome = tmux.attach(&session.tmux_session, Some(&session.tmux_window));
    match ratatui::try_init() {
        Ok(t) => {
            *terminal = t;
            // Coming back from tmux the screen holds its last frame.
            let _ = terminal.clear();
        }
        Err(e) => {
            // Without a terminal there is nothing left to draw on.
            restore_and_clear_terminal();
            eprintln!("cannot re-initialise the terminal after attach: {e}");
            std::process::exit(crate::cli::commands::dashboard::EXIT_NO_TTY);
        }
    }
    if let Err(e) = outcome {
        app.status_line = Some(format!("attach failed: {e:#}"));
    }
}

fn apply(
    action: Action,
    app: &mut DashboardApp,
    cfg: &Config,
    paths: &Paths,
    store: &Store,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<Flow> {
    match action {
        Action::Quit => return Ok(Flow::Quit),
        Action::Pause(id) => control(app, store, DaemonCommand::Pause { task_id: id }, "pause")?,
        Action::Resume(id) => control(app, store, DaemonCommand::Resume { task_id: id }, "resume")?,
        Action::Cancel(id) => control(app, store, DaemonCommand::Cancel { task_id: id }, "cancel")?,
        Action::Retry(id) => control(app, store, DaemonCommand::Retry { task_id: id }, "retry")?,
        Action::Attach(id) => match app.snapshot.latest_session(id).cloned() {
            None => app.status_line = Some("no session to attach to yet".into()),
            Some(s) if s.is_headless() => {
                app.status_line = Some(format!("{} runs headless: no window; see `powerqueue attach` for its log", s.id));
            }
            Some(s) if s.tmux_window.trim().is_empty() => {
                app.status_line = Some("the session has no tmux window recorded".into());
            }
            Some(s) => attach(app, cfg, terminal, &s),
        },
        Action::Refresh | Action::ToggleTerminal | Action::Help | Action::OpenAdd => {}
        Action::OpenProject(id) => {
            if let Some(mut task) = store.get_task(id)? {
                let project = task.execution_project.clone();
                match crate::scheduler::commands::on_set_project(&mut task, &project, store.latest_session(id)?.is_some()) {
                    Ok(_) => {
                        let projects: Vec<_> = cfg.project_ids().into_iter().collect();
                        let selected = projects.iter().position(|p| *p == project).unwrap_or(0);
                        app.project_picker = Some(app::ProjectPicker { task_id: id, projects, selected });
                    }
                    Err(e) => app.status_line = Some(e.to_string()),
                }
            }
        }
        Action::SetProject(id, project) => {
            let result = (|| -> Result<()> {
                Config::load(paths)?.for_project(&project, paths)?;
                let mut task = store.get_task(id)?.ok_or_else(|| anyhow::anyhow!("task no longer exists"))?;
                let from = task.execution_project.clone();
                let effects =
                    crate::scheduler::commands::on_set_project(&mut task, &project, store.latest_session(id)?.is_some())?;
                if app.snapshot.daemon_alive {
                    store.enqueue_command(&DaemonCommand::SetProject { task_id: id, project: project.clone() })?;
                    app.status_line = Some(format!("repository change to {project} queued for {}", task.key));
                } else {
                    store.set_execution_project(id, &from, &project)?;
                    for effect in effects {
                        if let crate::scheduler::transitions::Effect::Log { level, kind, message, data } = effect {
                            store.log_event(Some(id), None, level, &kind, &message, data)?;
                        }
                    }
                    app.status_line = Some(format!("{} repository: {project}", task.key));
                }
                Ok(())
            })();
            if let Err(e) = result {
                app.status_line = Some(format!("repository change failed: {e:#}"));
            }
        }
        Action::AddTask(title) => {
            let project = app.project_filter.clone().unwrap_or_else(|| {
                if cfg.routing.mode == crate::routing::RoutingMode::Deterministic { "default".into() } else { String::new() }
            });
            match create_task_in_config(store, &NewTask { title, execution_project: Some(project), ..Default::default() }, cfg) {
                Ok(t) => {
                    app.status_line = Some(format!("added {} ({})", t.key, t.id.short()));
                    if cfg.routing.mode == crate::routing::RoutingMode::Explicit && t.execution_project.is_empty() {
                        app.project_picker = Some(app::ProjectPicker {
                            task_id: t.id,
                            projects: cfg.project_ids().into_iter().collect(),
                            selected: 0,
                        });
                    }
                }
                Err(e) => app.status_line = Some(format!("add failed: {e:#}")),
            }
        }
    }
    Ok(Flow::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Task, TaskSource, TaskState};

    #[test]
    fn precheck_requires_ttys_and_a_term() {
        assert_eq!(precheck(true, true, Some("xterm-ghostty")), Ok(()));
        assert_eq!(precheck(false, true, Some("xterm")), Err(TerminalError::NotATty));
        assert_eq!(precheck(true, false, Some("xterm")), Err(TerminalError::NotATty));
        assert_eq!(precheck(true, true, None), Err(TerminalError::BadTerm("not set".into())));
        assert_eq!(precheck(true, true, Some("  ")), Err(TerminalError::BadTerm("not set".into())));
        assert_eq!(precheck(true, true, Some("dumb")), Err(TerminalError::BadTerm("`dumb`".into())));
        let msg = TerminalError::NotATty.to_string();
        assert!(msg.contains("stdin/stdout are not a TTY") && msg.contains("powerqueue status"), "{msg}");
        assert!(TerminalError::BadTerm("`dumb`".into()).to_string().contains("TERM is `dumb`"));
    }

    #[test]
    fn locale_detection() {
        assert!(locale_is_utf8(None, None, None), "unset locale assumes UTF-8");
        assert!(locale_is_utf8(None, None, Some("en_US.UTF-8")));
        assert!(locale_is_utf8(None, Some("C.utf8"), Some("C")));
        assert!(!locale_is_utf8(None, None, Some("C")));
        assert!(!locale_is_utf8(None, None, Some("POSIX")));
        assert!(!locale_is_utf8(Some("en_US.ISO8859-1"), None, Some("en_US.UTF-8")), "LC_ALL wins");
        assert!(locale_is_utf8(Some(""), None, Some("en_US.UTF-8")), "empty LC_ALL is ignored");
    }

    #[test]
    fn render_once_is_plain_text() {
        let mut t = Task::new("ENG-7", "Ship the dashboard", TaskSource::Manual);
        t.state = TaskState::Queued;
        let snapshot = Snapshot { tasks: vec![t], max_concurrent: 2, ..Default::default() };
        let text = render_once(snapshot.clone(), Options { ascii: true, ..Default::default() }, 100, 24);
        assert!(text.contains("powerqueue"), "{text}");
        assert!(text.contains("ENG-7"), "{text}");
        assert!(text.contains("+-"), "ascii borders: {text}");
        assert!(text.is_ascii(), "ascii mode must not emit non-ASCII glyphs: {text}");
        assert_eq!(text.lines().count(), 24);
        assert!(!text.lines().any(|l| l.ends_with(' ')), "trailing spaces trimmed");
        let unicode = render_once(snapshot, Options { ascii: false, ..Default::default() }, 100, 24);
        assert!(unicode.contains('┌'), "{unicode}");
        // Degenerate sizes are clamped, not panicking.
        assert_ne!(render_once(Snapshot::default(), Options::default(), 0, 0), "");
    }

    fn github_task(repository: &str, number: u64, project: &str, title: &str) -> Task {
        let source = TaskSource::GitHub { repository: repository.into(), number, url: String::new() };
        let mut t = Task::new(format!("{repository}#{number}"), title, source);
        t.state = TaskState::Queued;
        t.execution_project = project.into();
        t
    }

    #[test]
    fn render_once_tells_github_tasks_apart_at_80_columns() {
        let tasks = vec![
            github_task("aleandros/powerqueue", 83, "powerqueue", "Dashboard keys"),
            github_task("aleandros/acerosvs", 83, "acerosvs", "Invoice export"),
            github_task("aleandros/powerqueue", 84, "powerqueue", "Budget pacing"),
        ];
        // A hidden (cancelled) task from another owner does not bring owners back.
        let mut cancelled = github_task("alice/powerqueue", 5, "fork", "Old fork");
        cancelled.state = TaskState::Cancelled;
        let shown = [tasks.clone(), vec![cancelled]].concat();
        let snapshot = Snapshot { tasks: shown, max_concurrent: 2, ..Default::default() };
        let text = render_once(snapshot, Options { ascii: true, ..Default::default() }, 80, 12);
        // The table is the left 62% of the frame; the header line carries the clock.
        let table: Vec<String> = text.lines().skip(1).take(5).map(|l| l.chars().take(50).collect()).collect();
        assert_eq!(
            table,
            [
                "+ tasks (3 open) --------------------------------+",
                "|  KEY           QUEUE   STATE  MODEL TITLE      |",
                "|> powerqueue#83 default queued -     Dashboar...|",
                "|  acerosvs#83   default queued -     Invoice ...|",
                "|  powerqueue#84 default queued -     Budget p...|",
            ],
            "{text}"
        );
        // Only the table is shortened: the selected task keeps its full key.
        assert!(text.contains("|aleandros/powerqueue#83"), "{text}");
        assert_eq!(tasks[0].key, "aleandros/powerqueue#83");
        // An unrouted task (empty project) keeps the PROJECT column visible.
        let mut unrouted = tasks.clone();
        unrouted[1].execution_project.clear();
        let text = render_once(
            Snapshot { tasks: unrouted, max_concurrent: 2, ..Default::default() },
            Options { ascii: true, ..Default::default() },
            80,
            12,
        );
        assert!(text.lines().nth(2).is_some_and(|l| l.contains("PROJECT")), "{text}");

        // Same repository name under two owners: the owner comes back. A project
        // that is not the repository's name keeps the PROJECT column, and a
        // narrow table cuts labels in the middle, never the number.
        let mut tasks = tasks;
        tasks.push(github_task("alice/powerqueue", 83, "fork", "Fork sync"));
        let snapshot = Snapshot { tasks, max_concurrent: 2, ..Default::default() };
        let text = render_once(snapshot, Options { ascii: true, ..Default::default() }, 80, 12);
        let table: Vec<String> = text.lines().skip(2).take(5).map(|l| l.chars().take(50).collect()).collect();
        assert_eq!(
            table,
            [
                "|  KEY              PROJEC QUEU STATE  MODEL TITL|",
                "|> alean...queue#83 pow... d... queued -     D...|",
                "|  acerosvs#83      ace... d... queued -     I...|",
                "|  alean...queue#84 pow... d... queued -     B...|",
                "|  alice...queue#83 fork   d... queued -     F...|",
            ],
            "{text}"
        );
    }

    #[test]
    fn render_once_preserves_long_issue_numbers_and_manual_prefixes() {
        let mut github = github_task("owner/long-repository-name", 123_456_789, "execution-project", "GitHub issue");
        github.state = TaskState::NeedsAttention;
        let manual = Task::new("MANUAL-long-key#987654321", "Manual task", TaskSource::Manual);
        let text = render_once(
            Snapshot { tasks: vec![github, manual], ..Default::default() },
            Options { ascii: true, ..Default::default() },
            80,
            12,
        );
        let table: Vec<String> = text.lines().skip(3).take(2).map(|line| line.chars().take(50).collect()).collect();
        assert!(table.iter().any(|line| line.contains("#123456789")), "{text}");
        assert!(table.iter().any(|line| line.contains("MANUAL-")), "{text}");
        assert!(!table.iter().any(|line| line.contains("#987654321")), "manual keys retain right clipping: {text}");
    }

    #[test]
    fn project_filter_removes_unneeded_owners() {
        let snapshot = Snapshot {
            tasks: vec![github_task("alice/app", 83, "app", "Visible"), github_task("bob/app", 83, "fork", "Hidden")],
            ..Default::default()
        };
        let mut app = DashboardApp::new(snapshot.clone());
        assert_eq!(app.rows()[0].label, "alice/app#83");
        app.project_filter = Some("app".into());
        assert_eq!(app.rows()[0].label, "app#83");
        let text = render_once(snapshot, Options { ascii: true, project: Some("app".into()) }, 80, 12);
        let row: String = text.lines().nth(3).unwrap().chars().take(50).collect();
        assert!(row.contains("app#83") && !row.contains("alice/"), "{text}");
        assert!(text.contains("alice/app#83"), "detail preserves full key: {text}");
    }

    #[test]
    fn render_once_disambiguates_colliding_truncated_names() {
        for repositories in
            [["alice-dev/powerqueue", "alice-prod/powerqueue"], ["owner/web-infrastructure", "owner/api-infrastructure"]]
        {
            for ascii in [true, false] {
                let tasks = repositories.iter().map(|repo| github_task(repo, 83, "execution", "Same title")).collect();
                let text = render_once(Snapshot { tasks, ..Default::default() }, Options { ascii, ..Default::default() }, 80, 12);
                let rows: Vec<String> =
                    text.lines().skip(3).take(2).map(|line| line.chars().skip(3).take(16).collect()).collect();
                assert_ne!(rows[0], rows[1], "{text}");
                assert!(rows.iter().all(|row| row.contains("#83")), "{text}");
            }
        }
    }

    #[test]
    fn render_once_keeps_every_issue_digit_when_metadata_must_shrink() {
        for width in [60, 80] {
            for number in [1_234_567, u64::MAX] {
                let task = github_task("owner/repo", number, "different-project", "Long issue number");
                let text = render_once(
                    Snapshot { tasks: vec![task], ..Default::default() },
                    Options { ascii: true, ..Default::default() },
                    width,
                    12,
                );
                let table_width = (usize::from(width) * 62 + 50) / 100;
                let row: String = text.lines().nth(3).unwrap().chars().take(table_width).collect();
                assert!(row.contains(&format!("#{number}")), "{text}");
            }
        }
    }

    #[test]
    fn render_once_disambiguates_three_repositories_with_shared_prefixes_and_suffixes() {
        let repositories = [
            "owner/api-production-east-infrastructure",
            "owner/api-production-west-infrastructure",
            "owner/web-production-infrastructure",
        ];
        for ascii in [true, false] {
            let tasks = repositories.iter().map(|repo| github_task(repo, 83, "execution", "Same title")).collect();
            let text = render_once(Snapshot { tasks, ..Default::default() }, Options { ascii, ..Default::default() }, 80, 12);
            let keys: std::collections::BTreeSet<String> =
                text.lines().skip(3).take(3).map(|line| line.chars().skip(3).take(16).collect()).collect();
            assert_eq!(keys.len(), 3, "{text}");
            assert!(keys.iter().all(|key| key.contains("#83")), "{text}");
        }
    }
}
