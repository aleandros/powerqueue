//! Rendering helpers (pure functions of `DashboardApp`).

use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap};

use crate::cli::output::human_duration;
use crate::domain::{Criticality, ModelTier, TaskState};

use super::app::DashboardApp;

/// Footer key legend.
pub const KEYS: &str = "↑↓ select  a attach  p pause  r resume  c cancel  R retry  n new task  t terminal  ? help  q quit";

/// Colour of a task state; mirrors `cli::output::state_colored`.
pub fn state_style(state: TaskState) -> Style {
    match state {
        TaskState::Running => Style::default().fg(Color::Green),
        TaskState::Queued | TaskState::Starting => Style::default().fg(Color::Cyan),
        TaskState::Idle => Style::default().fg(Color::Yellow),
        TaskState::Crashed | TaskState::Failed => Style::default().fg(Color::Red),
        TaskState::Throttled | TaskState::Paused => Style::default().fg(Color::Magenta),
        TaskState::NeedsAttention => Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD),
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

/// Colour of a model tier; mirrors `cli::output::model_colored`.
pub fn model_style(m: Option<ModelTier>) -> Style {
    match m {
        Some(ModelTier::Fable) => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        Some(ModelTier::Opus) => Style::default().fg(Color::Blue),
        Some(ModelTier::Sonnet) => Style::default().fg(Color::Cyan),
        Some(ModelTier::Haiku) => Style::default().fg(Color::DarkGray),
        None => Style::default().fg(Color::DarkGray),
    }
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
    draw_table(frame, app, main[0]);
    let tiers = app.snapshot.ledger.as_ref().map(|l| l.tiers.len()).unwrap_or(ModelTier::ALL.len()) as u16;
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(tiers + 2), Constraint::Min(5)])
        .split(main[1]);
    draw_budget(frame, app, right[0]);
    draw_detail(frame, app, right[1]);
    draw_footer(frame, app, chunks[2]);
    if let Some(input) = &app.add_input {
        draw_add_modal(frame, input, area);
    }
    if app.show_help {
        draw_help(frame, area);
    }
}

fn draw_header(frame: &mut Frame, app: &DashboardApp, area: Rect) {
    let s = &app.snapshot;
    let daemon = if s.daemon_alive {
        Span::styled(format!("● daemon pid {}", s.daemon_pid.unwrap_or(0)), Style::default().fg(Color::Green))
    } else {
        Span::styled("● daemon not running", Style::default().fg(Color::Red))
    };
    let mut spans = vec![
        Span::styled(" powerqueue ", Style::default().add_modifier(Modifier::BOLD)),
        daemon,
        Span::raw(format!("  slots {}/{}", s.slots_used(), s.max_concurrent)),
    ];
    match &s.ledger {
        Some(l) => {
            let period = l.period_fraction();
            let elapsed = l.elapsed_fraction();
            spans.push(Span::raw("  period "));
            spans.push(Span::styled(format!("{:.0}%", period * 100.0), Style::default().fg(pacing_color(period, elapsed))));
            spans.push(Span::raw(format!(
                " (elapsed {:.0}%, {} left)",
                elapsed * 100.0,
                human_duration(l.period.remaining(l.now).num_seconds())
            )));
            let window = l.window_fraction();
            spans.push(Span::raw("  window "));
            spans.push(Span::styled(
                format!("{:.0}%", window * 100.0),
                Style::default().fg(if window > 0.9 {
                    Color::Red
                } else if window > 0.7 {
                    Color::Yellow
                } else {
                    Color::Green
                }),
            ));
        }
        None => spans.push(Span::styled("  budget: n/a", Style::default().fg(Color::DarkGray))),
    }
    let when = s.taken_at.unwrap_or_else(Utc::now).with_timezone(&chrono::Local).format("%H:%M:%S").to_string();
    let left = Line::from(spans);
    let right = Line::from(Span::styled(format!("{when} "), Style::default().fg(Color::DarkGray))).right_aligned();
    frame.render_widget(Paragraph::new(left), area);
    frame.render_widget(Paragraph::new(right), area);
}

fn draw_table(frame: &mut Frame, app: &DashboardApp, area: Rect) {
    let rows = app.rows();
    let title_width = area.width.saturating_sub(2 + 12 + 11 + 9 + 7 + 7 + 5 + 9 + 7 + 8).max(10);
    let header =
        Row::new(["KEY", "STATE", "CRIT", "MODEL", "TOKENS", "CPU", "RSS", "AGE", "TITLE"].map(|h| Cell::from(h).bold()))
            .style(Style::default().fg(Color::DarkGray));
    let body = rows.iter().map(|r| {
        Row::new(vec![
            Cell::from(r.key.clone()),
            Cell::from(r.state.as_str()).style(state_style(r.state)),
            Cell::from(r.criticality.as_str()).style(criticality_style(r.criticality)),
            Cell::from(r.model.map(|m| m.alias()).unwrap_or("-")).style(model_style(r.model)),
            Cell::from(r.tokens.clone()),
            Cell::from(r.cpu.clone()),
            Cell::from(r.rss.clone()),
            Cell::from(r.age.clone()),
            Cell::from(crate::cli::output::truncate(&r.title, title_width as usize)),
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
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(4),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(title))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("▶ ");
    let mut state = TableState::default().with_selected(if count == 0 { None } else { Some(app.selected.min(count - 1)) });
    frame.render_stateful_widget(table, area, &mut state);
    if count == 0 {
        let msg = Paragraph::new(Line::from(Span::styled(
            "no tasks — press n to add one, or let the daemon sync Linear",
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: true });
        let inner = Rect { x: area.x + 2, y: area.y + 2, width: area.width.saturating_sub(4), height: 2 };
        frame.render_widget(msg, inner);
    }
}

fn draw_budget(frame: &mut Frame, app: &DashboardApp, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" budget ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(ledger) = &app.snapshot.ledger else {
        frame.render_widget(
            Paragraph::new("ledger unavailable (run `powerqueue doctor`)").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    let elapsed = ledger.elapsed_fraction();
    let tiers: Vec<_> = ModelTier::ALL.iter().map(|t| ledger.tier(*t)).collect();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(tiers.iter().map(|_| Constraint::Length(1)).collect::<Vec<_>>())
        .split(inner);
    for (i, tier) in tiers.iter().enumerate() {
        let share = if ledger.period_budget > 0.0 { tier.period_budget / ledger.period_budget } else { 0.0 };
        let share = app.snapshot.shares.get(&tier.tier).copied().unwrap_or(share);
        let spent_of_total = if ledger.period_budget > 0.0 { tier.period_weighted / ledger.period_budget } else { 0.0 };
        let spent = tier.period_spent_fraction();
        let label = format!("{:<6} {:>3.0}% / {:>3.0}%", tier.tier.alias(), spent_of_total * 100.0, share * 100.0);
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(pacing_color(spent, elapsed)).bg(Color::Black))
            .ratio(spent.clamp(0.0, 1.0))
            .label(label);
        if let Some(r) = rows.get(i) {
            frame.render_widget(gauge, *r);
        }
    }
}

fn draw_detail(frame: &mut Frame, app: &DashboardApp, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" selected task ");
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
    match app.snapshot.latest_session(task.id) {
        Some(s) => {
            let started = s.started_at.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string();
            lines.push(Line::from(vec![
                Span::raw(format!("session #{} ", s.attempt)),
                Span::styled(s.model.alias(), model_style(Some(s.model))),
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

fn draw_footer(frame: &mut Frame, app: &DashboardApp, area: Rect) {
    let line = match &app.status_line {
        Some(s) => Line::from(vec![
            Span::styled(format!(" {s}"), Style::default().fg(Color::Yellow)),
            Span::styled(format!("   {KEYS}"), Style::default().fg(Color::DarkGray)),
        ]),
        None => Line::from(Span::styled(format!(" {KEYS}"), Style::default().fg(Color::DarkGray))),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_add_modal(frame: &mut Frame, input: &str, area: Rect) {
    let rect = centered(area, 60, 5);
    frame.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title(" new task — title (enter to add, esc to cancel) ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let text =
        Paragraph::new(Line::from(vec![Span::raw(format!("> {input}")), Span::styled("█", Style::default().fg(Color::Yellow))]));
    frame.render_widget(text, inner);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let lines = [
        "↑/k ↓/j   move selection      g/G  first/last",
        "a/enter   attach to the task's tmux window",
        "p / r     pause / resume      c    cancel",
        "R         retry (re-queue)    n    new manual task",
        "t         show/hide finished  u    refresh now",
        "?         this help           q    quit",
        "",
        "control commands go through the daemon; when it is not running",
        "they are applied directly where the state machine allows it.",
    ];
    let rect = centered(area, 66, lines.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title(" keys ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines.iter().map(|l| Line::from(*l)).collect::<Vec<_>>()), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::app::Snapshot;
    use crate::domain::{Task, TaskSource};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn smoke_render() {
        let mut t = Task::new("ENG-1", "Fix the login flow", TaskSource::Manual);
        t.state = TaskState::Running;
        let snapshot = Snapshot { tasks: vec![t], max_concurrent: 2, ..Default::default() };
        let mut app = DashboardApp::new(snapshot);
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("powerqueue"));
        assert!(text.contains("ENG-1"));
        assert!(text.contains("ledger unavailable"));
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
    fn pacing_colours() {
        assert_eq!(pacing_color(1.2, 0.5), Color::Red);
        assert_eq!(pacing_color(0.8, 0.5), Color::LightRed);
        assert_eq!(pacing_color(0.1, 0.5), Color::Blue);
        assert_eq!(pacing_color(0.5, 0.5), Color::Green);
    }
}
