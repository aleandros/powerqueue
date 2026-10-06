//! Rendering helpers (pure functions of `DashboardApp`).
//!
//! Every non-ASCII glyph goes through [`Symbols`] so `--ascii` (or a non-UTF-8
//! locale) can swap in `*`, `>`, `#` and `+-|` borders for terminals or fonts
//! that show boxes instead.

use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap};

use crate::domain::{Criticality, ModelTier, Provider, TaskState};

use super::app::DashboardApp;

/// Footer key legend (Unicode variant; see [`Symbols::keys`]).
pub const KEYS: &str = "↑↓ select  a attach  p pause  r resume  c cancel  R retry  n new task  t terminal  ? help  q quit";
/// Footer key legend in plain ASCII.
pub const KEYS_ASCII: &str =
    "up/down select  a attach  p pause  r resume  c cancel  R retry  n new task  t terminal  ? help  q quit";

/// `+-|` borders for terminals without box-drawing glyphs.
pub const ASCII_BORDER: border::Set = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

/// The glyphs the dashboard draws with, in Unicode or ASCII flavour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbols {
    pub ascii: bool,
    /// Daemon status dot.
    pub bullet: &'static str,
    /// Selected-row marker (two columns).
    pub pointer: &'static str,
    /// Text cursor in the add-task modal.
    pub cursor: &'static str,
    /// Em dash in messages.
    pub dash: &'static str,
    /// Footer legend.
    pub keys: &'static str,
    /// Up/down arrows in the help overlay.
    pub arrows: &'static str,
    /// Suffix of truncated titles.
    pub ellipsis: &'static str,
    /// "Not applicable" marker (a provider without a window).
    pub na: &'static str,
    pub border: border::Set<'static>,
}

impl Symbols {
    /// Unicode glyphs, or ASCII look-alikes when `ascii`.
    pub const fn for_mode(ascii: bool) -> Self {
        if ascii {
            Self {
                ascii: true,
                bullet: "*",
                pointer: "> ",
                cursor: "#",
                dash: "-",
                keys: KEYS_ASCII,
                arrows: "up/dn",
                ellipsis: "...",
                na: "-",
                border: ASCII_BORDER,
            }
        } else {
            Self {
                ascii: false,
                bullet: "●",
                pointer: "▶ ",
                cursor: "█",
                dash: "—",
                keys: KEYS,
                arrows: "↑/↓  ",
                ellipsis: "…",
                na: "–",
                border: border::PLAIN,
            }
        }
    }

    /// A bordered panel with this symbol set.
    pub fn panel(&self, title: String) -> Block<'static> {
        Block::default().borders(Borders::ALL).border_set(self.border).title(title)
    }

    /// Truncate `s` to `max` columns, ending with this set's ellipsis.
    pub fn truncate(&self, s: &str, max: usize) -> String {
        use unicode_width::UnicodeWidthStr;
        if s.width() <= max {
            return s.to_string();
        }
        let keep = max.saturating_sub(self.ellipsis.width());
        let mut out = String::new();
        let mut width = 0;
        for ch in s.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if width + w > keep {
                break;
            }
            width += w;
            out.push(ch);
        }
        out.push_str(self.ellipsis);
        out
    }
}

/// Textual progress bar (`[####......]`) used instead of a colour gauge in
/// ASCII mode; `width` is the total width including the brackets.
pub fn text_bar(ratio: f64, width: usize) -> String {
    let inner = width.saturating_sub(2);
    let filled = ((ratio.clamp(0.0, 1.0) * inner as f64).round() as usize).min(inner);
    format!("[{}{}]", "#".repeat(filled), ".".repeat(inner - filled))
}

/// Colour of a task state; mirrors `cli::output::state_colored`.
pub fn state_style(state: TaskState) -> Style {
    match state {
        TaskState::Running => Style::default().fg(Color::Green),
        TaskState::Queued | TaskState::Starting => Style::default().fg(Color::Cyan),
        TaskState::Idle => Style::default().fg(Color::Yellow),
        TaskState::Crashed | TaskState::Failed => Style::default().fg(Color::Red),
        TaskState::Throttled | TaskState::Paused => Style::default().fg(Color::Magenta),
        TaskState::Blocked => Style::default().fg(Color::Blue),
        TaskState::NeedsAttention => Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD),
        TaskState::InReview => Style::default().fg(Color::LightBlue),
        TaskState::Completed => Style::default().fg(Color::LightGreen),
        TaskState::Cancelled => Style::default().fg(Color::DarkGray),
    }
}

/// Colour of a criticality; mirrors `cli::output::criticality_colored`.
pub fn criticality_style(c: Criticality) -> Style {
    match c {
        Criticality::Critical => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Criticality::High => Style::default().fg(Color::Yellow),
        Criticality::Normal => Style::default(),
        Criticality::Low => Style::default().fg(Color::DarkGray),
    }
}

/// Colour of a model; mirrors `cli::output::model_colored` (Claude tiers by
/// alias, Codex green, Gemini yellow).
pub fn model_style(m: Option<&ModelTier>) -> Style {
    let Some(m) = m else { return Style::default().fg(Color::DarkGray) };
    match (m.provider(), m.alias()) {
        (Provider::Claude, "fable") => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        (Provider::Claude, "opus") => Style::default().fg(Color::Blue),
        (Provider::Claude, "sonnet") => Style::default().fg(Color::Cyan),
        (Provider::Claude, "haiku") => Style::default().fg(Color::DarkGray),
        (Provider::Claude, _) => Style::default(),
        (Provider::Codex, _) => Style::default().fg(Color::Green),
        (Provider::Gemini, _) => Style::default().fg(Color::Yellow),
    }
}

/// Two-letter provider tag for the header summary (`cl 34/12%`).
pub fn provider_tag(p: Provider) -> &'static str {
    match p {
        Provider::Claude => "cl",
        Provider::Codex => "cx",
        Provider::Gemini => "gm",
    }
}

/// Colour of a window fraction: red when nearly spent, yellow when high.
pub fn window_color(window: f64) -> Color {
    if window > 0.9 {
        Color::Red
    } else if window > 0.7 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Rows the budget panel needs: one header row plus one gauge per model for
/// every provider (one row when there is nothing to show).
pub fn budget_rows(app: &DashboardApp) -> u16 {
    let ledgers = app.snapshot.ordered_ledgers();
    if ledgers.is_empty() {
        return 1;
    }
    ledgers.iter().map(|l| 1 + l.tiers.len()).sum::<usize>().min(u16::MAX as usize) as u16
}

/// Gauge colour from pacing: spent fraction vs elapsed fraction of the period.
pub fn pacing_color(spent: f64, elapsed: f64) -> Color {
    if spent >= 1.0 {
        Color::Red
    } else if spent > elapsed + 0.15 {
        Color::LightRed
    } else if spent + 0.15 < elapsed {
        Color::Blue
    } else {
        Color::Green
    }
}

/// Draw one frame.
pub fn draw(frame: &mut Frame, app: &DashboardApp) {
    let sym = Symbols::for_mode(app.ascii);
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(5), Constraint::Length(1)])
        .split(area);
    draw_header(frame, app, chunks[0]);
    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(chunks[1]);
    draw_table(frame, app, main[0], &sym);
    let rows = budget_rows(app);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(rows + 2), Constraint::Min(5)])
        .split(main[1]);
    draw_budget(frame, app, right[0], &sym);
    draw_detail(frame, app, right[1], &sym);
    draw_footer(frame, app, chunks[2], &sym);
    if let Some(input) = &app.add_input {
        draw_add_modal(frame, input, area, &sym);
    }
    if app.show_help {
        draw_help(frame, area, &sym);
    }
}

fn draw_header(frame: &mut Frame, app: &DashboardApp, area: Rect) {
    let sym = Symbols::for_mode(app.ascii);
    let s = &app.snapshot;
    let daemon = if s.daemon_alive {
        Span::styled(format!("{} daemon pid {}", sym.bullet, s.daemon_pid.unwrap_or(0)), Style::default().fg(Color::Green))
    } else {
        Span::styled(format!("{} daemon not running", sym.bullet), Style::default().fg(Color::Red))
    };
    let mut spans = vec![
        Span::styled(" powerqueue ", Style::default().add_modifier(Modifier::BOLD)),
        daemon,
        Span::raw(format!("  slots {}/{}", s.slots_used(), s.max_concurrent)),
    ];
    if let Some(p) = &s.scheduling_pause {
        spans.push(Span::styled(format!("  scheduling {}", p.describe()), Style::default().fg(Color::Yellow)));
    }
    let (review, manual) = s.in_review();
    if review > 0 {
        spans.push(Span::styled(format!("  in review {review}"), state_style(TaskState::InReview)));
        if manual > 0 {
            spans.push(Span::styled(format!(" ({manual} waiting for manual merge)"), Style::default().fg(Color::DarkGray)));
        }
    }
    let ledgers = s.ordered_ledgers();
    if ledgers.is_empty() {
        spans.push(Span::styled("  budget: n/a", Style::default().fg(Color::DarkGray)));
    } else {
        // Compact per-provider summary: `cl 34/12%` = period / window spent.
        spans.push(Span::raw(" "));
        for l in &ledgers {
            let period = l.period_fraction();
            let elapsed = l.elapsed_fraction();
            let window = if l.window_enabled { format!("{:.0}", l.window_fraction() * 100.0) } else { sym.na.to_string() };
            spans.push(Span::raw(format!(" {} ", provider_tag(l.provider))));
            spans.push(Span::styled(
                format!("{:.0}/{window}%", period * 100.0),
                Style::default().fg(pacing_color(period, elapsed)),
            ));
        }
        spans.push(Span::raw("  next "));
        match &s.next_model {
            Some(m) => spans.push(Span::styled(m.as_str().to_string(), model_style(Some(m)))),
            None => spans.push(Span::styled("none", Style::default().fg(Color::DarkGray))),
        }
    }
    let when = s.taken_at.unwrap_or_else(Utc::now).with_timezone(&chrono::Local).format("%H:%M:%S").to_string();
    let left = Line::from(spans);
    let right = Line::from(Span::styled(format!("{when} "), Style::default().fg(Color::DarkGray))).right_aligned();
    frame.render_widget(Paragraph::new(left), area);
    frame.render_widget(Paragraph::new(right), area);
}

fn draw_table(frame: &mut Frame, app: &DashboardApp, area: Rect, sym: &Symbols) {
    let rows = app.rows();
    let title_width = area.width.saturating_sub(2 + 12 + 11 + 9 + 13 + 7 + 5 + 9 + 7 + 13 + 8).max(10);
    let header = Row::new(
        ["KEY", "STATE", "CRIT", "MODEL", "WTOK", "CPU", "RSS", "AGE", "WAITING ON", "TITLE"].map(|h| Cell::from(h).bold()),
    )
    .style(Style::default().fg(Color::DarkGray));
    let body = rows.iter().map(|r| {
        Row::new(vec![
            Cell::from(r.key.clone()),
            Cell::from(r.state.as_str()).style(state_style(r.state)),
            Cell::from(r.criticality.as_str()).style(criticality_style(r.criticality)),
            Cell::from(r.model.as_ref().map(|m| m.as_str()).unwrap_or("-")).style(model_style(r.model.as_ref())),
            Cell::from(r.tokens.clone()),
            Cell::from(r.cpu.clone()),
            Cell::from(r.rss.clone()),
            Cell::from(r.age.clone()),
            Cell::from(sym.truncate(&r.waiting, 12)).style(if r.waiting == "-" {
                Style::default().fg(Color::DarkGray)
            } else {
                state_style(TaskState::Blocked)
            }),
            Cell::from(sym.truncate(&r.title, title_width as usize)),
        ])
    });
    let count = rows.len();
    let title = if app.filter_terminal { format!(" tasks ({count} open) ") } else { format!(" tasks ({count}, all) ") };
    let table = Table::new(
        body,
        [
            Constraint::Length(12),
            Constraint::Length(11),
            Constraint::Length(8),
            // Wide enough for Codex / Gemini names (`gpt-6.1-sol`).
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(4),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(sym.panel(title))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol(sym.pointer);
    let mut state = TableState::default().with_selected(if count == 0 { None } else { Some(app.selected.min(count - 1)) });
    frame.render_stateful_widget(table, area, &mut state);
    if count == 0 {
        let msg = Paragraph::new(Line::from(Span::styled(
            format!("no tasks {} press n to add one, or let the daemon sync Linear", sym.dash),
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: true });
        let inner = Rect { x: area.x + 2, y: area.y + 2, width: area.width.saturating_sub(4), height: 2 };
        frame.render_widget(msg, inner);
    }
}

fn draw_budget(frame: &mut Frame, app: &DashboardApp, area: Rect, sym: &Symbols) {
    let block = sym.panel(" budget ".into());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let ledgers = app.snapshot.ordered_ledgers();
    if ledgers.is_empty() {
        frame.render_widget(
            Paragraph::new("ledger unavailable (run `powerqueue doctor`)").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints((0..budget_rows(app)).map(|_| Constraint::Length(1)).collect::<Vec<_>>())
        .split(inner);
    let mut next_row = 0usize;
    for ledger in ledgers {
        let Some(r) = rows.get(next_row) else { break };
        next_row += 1;
        frame.render_widget(Paragraph::new(provider_header(app, ledger, sym)), *r);
        let elapsed = ledger.elapsed_fraction();
        let name_width = ledger.tiers.iter().map(|t| t.tier.alias().len()).max().unwrap_or(6).clamp(6, 12);
        for tier in &ledger.tiers {
            let Some(r) = rows.get(next_row) else { break };
            next_row += 1;
            let share = if ledger.period_budget > 0.0 { tier.period_budget / ledger.period_budget } else { 0.0 };
            let share = app.snapshot.shares.get(&tier.tier).copied().unwrap_or(share);
            let spent_of_total = if ledger.period_budget > 0.0 { tier.period_weighted / ledger.period_budget } else { 0.0 };
            let spent = tier.period_spent_fraction();
            let label = format!(
                "{:<name_width$} {:>3.0}% / {:>3.0}%",
                sym.truncate(tier.tier.alias(), name_width),
                spent_of_total * 100.0,
                share * 100.0
            );
            let colour = pacing_color(spent, elapsed);
            if sym.ascii {
                // A textual bar survives plain-text dumps and fonts without block glyphs.
                let bar = text_bar(spent, (r.width as usize).saturating_sub(label.len() + 1).clamp(4, 30));
                let line = Line::from(vec![Span::raw(format!("{label} ")), Span::styled(bar, Style::default().fg(colour))]);
                frame.render_widget(Paragraph::new(line), *r);
            } else {
                // No hard-coded background: the unfilled part keeps the terminal's
                // own colours so it does not show up as black blocks on light themes.
                let gauge = Gauge::default().gauge_style(Style::default().fg(colour)).ratio(spent.clamp(0.0, 1.0)).label(label);
                frame.render_widget(gauge, *r);
            }
        }
    }
}

/// The dim header row of a provider block:
/// `claude  period 34% (elapsed 59%)  window 12%  [cooldown until 14:30]`.
fn provider_header(app: &DashboardApp, ledger: &crate::budget::Ledger, sym: &Symbols) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let period = ledger.period_fraction();
    let elapsed = ledger.elapsed_fraction();
    let mut spans = vec![
        Span::styled(format!("{:<7}", ledger.provider), dim.add_modifier(Modifier::BOLD)),
        Span::styled(" period ".to_string(), dim),
        Span::styled(format!("{:.0}%", period * 100.0), Style::default().fg(pacing_color(period, elapsed))),
        Span::styled(format!(" (elapsed {:.0}%)", elapsed * 100.0), dim),
        Span::styled("  window ".to_string(), dim),
    ];
    if ledger.window_enabled {
        let window = ledger.window_fraction();
        spans.push(Span::styled(format!("{:.0}%", window * 100.0), Style::default().fg(window_color(window))));
    } else {
        spans.push(Span::styled(sym.na.to_string(), dim));
    }
    if let Some(until) = app.snapshot.cooldowns.get(&ledger.provider) {
        let when = until.with_timezone(&chrono::Local).format("%H:%M").to_string();
        spans.push(Span::styled(format!("  [cooldown until {when}]"), Style::default().fg(Color::Magenta)));
    }
    Line::from(spans)
}

fn draw_detail(frame: &mut Frame, app: &DashboardApp, area: Rect, sym: &Symbols) {
    let block = sym.panel(" selected task ".into());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(task) = app.selected_task() else {
        frame.render_widget(Paragraph::new("nothing selected").style(Style::default().fg(Color::DarkGray)), inner);
        return;
    };
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(task.key.clone(), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::raw(task.title.clone()),
        ]),
        Line::from(vec![
            Span::raw("state "),
            Span::styled(task.state.as_str(), state_style(task.state)),
            Span::raw("  crit "),
            Span::styled(task.criticality.as_str(), criticality_style(task.criticality)),
            Span::raw(format!("  score {:.0}  attempts {}", task.score, task.attempts)),
        ]),
    ];
    if let Some(b) = &task.branch {
        lines.push(Line::from(Span::styled(format!("branch {b}"), Style::default().fg(Color::DarkGray))));
    }
    if let Some(e) = &task.last_error {
        lines.push(Line::from(Span::styled(format!("error {e}"), Style::default().fg(Color::Red))));
    }
    if task.state == TaskState::NeedsAttention {
        lines.push(Line::from(Span::styled("press Enter to attach and respond", Style::default().fg(Color::Yellow))));
    }
    match app.snapshot.latest_session(task.id) {
        Some(s) => {
            let started = s.started_at.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string();
            lines.push(Line::from(vec![
                Span::raw(format!("session #{} ", s.attempt)),
                Span::styled(s.model.as_str(), model_style(Some(&s.model))),
                Span::raw(format!(" {} started {started}", s.state)),
                Span::raw(s.pane_id.as_ref().map(|p| format!(" pane {p}")).unwrap_or_default()),
            ]));
        }
        None => lines.push(Line::from(Span::styled("no session yet", Style::default().fg(Color::DarkGray)))),
    }
    lines.push(Line::from(Span::styled("recent events", Style::default().add_modifier(Modifier::UNDERLINED))));
    let events = app.snapshot.events_for(task.id, 10);
    if events.is_empty() {
        lines.push(Line::from(Span::styled("(none)", Style::default().fg(Color::DarkGray))));
    }
    for e in events {
        let when = e.timestamp.with_timezone(&chrono::Local).format("%H:%M:%S").to_string();
        let level_style = match e.level {
            crate::domain::EventLevel::Error => Style::default().fg(Color::Red),
            crate::domain::EventLevel::Warn => Style::default().fg(Color::Yellow),
            _ => Style::default().fg(Color::DarkGray),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{when} "), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{} ", e.kind), level_style),
            Span::raw(e.message.clone()),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_footer(frame: &mut Frame, app: &DashboardApp, area: Rect, sym: &Symbols) {
    let keys = sym.keys;
    let attention = app.snapshot.tasks.iter().filter(|t| t.state == TaskState::NeedsAttention).count();
    let notice = (attention > 0).then(|| format!("{attention} need attention: select task + Enter to respond"));
    let line = match app.status_line.as_ref().or(notice.as_ref()) {
        Some(s) => Line::from(vec![
            Span::styled(format!(" {s}"), Style::default().fg(Color::Yellow)),
            Span::styled(format!("   {keys}"), Style::default().fg(Color::DarkGray)),
        ]),
        None => Line::from(Span::styled(format!(" {keys}"), Style::default().fg(Color::DarkGray))),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_add_modal(frame: &mut Frame, input: &str, area: Rect, sym: &Symbols) {
    let rect = centered(area, 60, 5);
    frame.render_widget(Clear, rect);
    let block = sym.panel(format!(" new task {} title (enter to add, esc to cancel) ", sym.dash));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let text = Paragraph::new(Line::from(vec![
        Span::raw(format!("> {input}")),
        Span::styled(sym.cursor, Style::default().fg(Color::Yellow)),
    ]));
    frame.render_widget(text, inner);
}

fn draw_help(frame: &mut Frame, area: Rect, sym: &Symbols) {
    let lines = [
        "WTOK: cumulative weighted tokens, not context or quota %".to_string(),
        format!("{} k/j  move selection      g/G  first/last", sym.arrows),
        "a/enter    attach to the task's tmux window".to_string(),
        "p / r      pause / resume      c    cancel".to_string(),
        "R          retry (re-queue)    n    new manual task".to_string(),
        "t          show/hide finished  u    refresh now".to_string(),
        "?          this help           q    quit".to_string(),
        String::new(),
        "control commands go through the daemon; when it is not running".to_string(),
        "they are applied directly where the state machine allows it.".to_string(),
    ];
    let rect = centered(area, 66, lines.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    let block = sym.panel(" keys ".into());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines.iter().map(|l| Line::from(l.as_str())).collect::<Vec<_>>()), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::app::Snapshot;
    use crate::domain::{Task, TaskSource};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// A snapshot with Claude and Codex enabled (empty ledgers) and one task.
    fn two_provider_snapshot(key: &str, title: &str) -> Snapshot {
        use crate::config::Config;
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.budget.providers.codex.enabled = true;
        let mut s = Snapshot::load(&store, &cfg, Utc::now()).unwrap();
        let mut t = Task::new(key, title, TaskSource::Manual);
        t.state = TaskState::Running;
        s.tasks = vec![t];
        s.max_concurrent = 2;
        s.cooldowns.insert(Provider::Codex, Utc::now() + chrono::Duration::minutes(20));
        s
    }

    #[test]
    fn smoke_render() {
        let mut t = Task::new("ENG-1", "Fix the login flow", TaskSource::Manual);
        t.state = TaskState::Running;
        let snapshot = Snapshot { tasks: vec![t], max_concurrent: 2, ..Default::default() };
        let app = DashboardApp::new(snapshot);
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("powerqueue"));
        assert!(text.contains("ENG-1"));
        assert!(text.contains("ledger unavailable"));
        assert!(text.contains("budget: n/a"));

        // Two providers: one block each with a header row and a gauge per model.
        let mut app = DashboardApp::new(two_provider_snapshot("ENG-1", "Fix the login flow"));
        assert_eq!(budget_rows(&app), 1 + 4 + 1 + 3);
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = crate::dashboard::buffer_to_text(terminal.backend().buffer());
        for needle in [
            "claude ",
            "codex ",
            "fable",
            "haiku",
            "gpt-6.1-sol",
            "gpt-6-luna",
            "cooldown until",
            " cl 0/0%",
            " cx 0/0%",
            "next sonnet",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        let claude_line = text.lines().position(|l| l.contains("claude ") && l.contains("period")).unwrap();
        let codex_line = text.lines().position(|l| l.contains("codex ") && l.contains("period")).unwrap();
        assert_eq!(codex_line - claude_line, 5, "claude's four gauges sit between the two headers:\n{text}");
        app.add_input = Some("new".into());
        app.show_help = true;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("keys"));
        // Tiny terminal must not panic.
        let mut tiny = Terminal::new(TestBackend::new(20, 6)).unwrap();
        tiny.draw(|f| draw(f, &app)).unwrap();
    }

    #[test]
    fn ascii_mode_emits_only_ascii() {
        let mut t = Task::new("ENG-2", "Ascii fallback — check", TaskSource::Manual);
        t.state = TaskState::Running;
        let snapshot = Snapshot { tasks: vec![t], max_concurrent: 2, ..Default::default() };
        let mut app = DashboardApp::new(snapshot);
        app.ascii = true;
        app.add_input = Some("x".into());
        app.show_help = true;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        let ours: String = text.chars().filter(|c| !c.is_ascii()).collect();
        // The only non-ASCII characters on screen come from user data (the title).
        assert_eq!(ours, "—", "{text}");
        assert!(text.contains("+-"), "ascii border: {text}");
        assert!(text.contains("* daemon not running"), "{text}");
        assert!(text.contains("> ENG-2"), "ascii pointer: {text}");
        assert!(text.contains("up/down select"), "{text}");
        app.show_help = false;
        app.add_input = None;
        app.ascii = false;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("● daemon not running") && text.contains("▶ ENG-2"), "{text}");
    }

    #[test]
    fn ascii_frame_stays_ascii_with_two_providers() {
        let mut app = DashboardApp::new(two_provider_snapshot("ENG-3", "Plain title"));
        app.ascii = true;
        let text = crate::dashboard::render_once_app(&app, 200, 40);
        assert!(text.is_ascii(), "non-ASCII in ASCII mode:\n{text}");
        assert!(text.contains("claude ") && text.contains("codex "), "{text}");
        assert!(text.contains("[#") || text.contains("[."), "textual bars: {text}");
        assert!(text.contains("cooldown until"), "{text}");
    }

    #[test]
    fn provider_tags_and_window_colour() {
        assert_eq!(provider_tag(Provider::Claude), "cl");
        assert_eq!(provider_tag(Provider::Codex), "cx");
        assert_eq!(provider_tag(Provider::Gemini), "gm");
        assert_eq!(window_color(0.95), Color::Red);
        assert_eq!(window_color(0.8), Color::Yellow);
        assert_eq!(window_color(0.1), Color::Green);
        assert!(Symbols::for_mode(true).na.is_ascii());
    }

    #[test]
    fn symbols_and_text_bar() {
        assert_eq!(Symbols::for_mode(true).pointer, "> ");
        assert_eq!(Symbols::for_mode(false).pointer, "▶ ");
        assert!(Symbols::for_mode(true).keys.is_ascii());
        assert_eq!(text_bar(0.0, 12), "[..........]");
        assert_eq!(text_bar(0.5, 12), "[#####.....]");
        assert_eq!(text_bar(1.5, 12), "[##########]");
        assert_eq!(text_bar(0.5, 1), "[]");
        let ascii = Symbols::for_mode(true);
        assert_eq!(ascii.truncate("short", 10), "short");
        assert_eq!(ascii.truncate("a long title here", 10), "a long ...");
        assert_eq!(Symbols::for_mode(false).truncate("a long title here", 10), "a long ti…");
    }

    #[test]
    fn pacing_colours() {
        assert_eq!(pacing_color(1.2, 0.5), Color::Red);
        assert_eq!(pacing_color(0.8, 0.5), Color::LightRed);
        assert_eq!(pacing_color(0.1, 0.5), Color::Blue);
        assert_eq!(pacing_color(0.5, 0.5), Color::Green);
    }
}
