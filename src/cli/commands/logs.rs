//! `powerqueue logs` — read the daemon log (or the event timeline).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use owo_colors::{OwoColorize, Stream, Style};

use crate::cli::{Context, LogsArgs};
use crate::logging::log_files;

use super::status::ensure_initialised;

/// Log severity order used by `--level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// Parse `trace|debug|info|warn|error` (case-insensitive, `warning` accepted).
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Level::Trace),
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

/// A parsed log line, from either the JSON or the text format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub time: String,
    pub level: Option<Level>,
    pub target: String,
    pub message: String,
    /// `k=v` pairs (structured fields other than `message`).
    pub fields: Vec<(String, String)>,
}

/// Parse one `tracing` JSON line (`{"timestamp":..,"level":..,"fields":{..},"target":..}`).
pub fn parse_json_line(line: &str) -> Option<LogLine> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let obj = v.as_object()?;
    let timestamp = obj.get("timestamp").and_then(|t| t.as_str()).unwrap_or("");
    let time = chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| timestamp.chars().take(8).collect());
    let level = obj.get("level").and_then(|l| l.as_str()).and_then(Level::parse);
    let target = obj.get("target").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let mut message = String::new();
    let mut fields = Vec::new();
    if let Some(f) = obj.get("fields").and_then(|f| f.as_object()) {
        for (k, val) in f {
            let text = val.as_str().map(str::to_string).unwrap_or_else(|| val.to_string());
            if k == "message" {
                message = text;
            } else {
                fields.push((k.clone(), text));
            }
        }
    }
    Some(LogLine { time, level, target, message, fields })
}

/// Render a JSON log line as `HH:MM:SS LEVEL target message k=v`; `None`
/// when the line is not JSON (text lines are passed through by the caller).
pub fn format_json_line(line: &str) -> Option<String> {
    let parsed = parse_json_line(line)?;
    Some(render(&parsed, false))
}

fn render(l: &LogLine, color: bool) -> String {
    let level = l.level.map(|lv| lv.as_str()).unwrap_or("-");
    let level_text = if color {
        match l.level {
            Some(Level::Error) => level.if_supports_color(Stream::Stdout, |t| t.style(Style::new().red().bold())).to_string(),
            Some(Level::Warn) => level.if_supports_color(Stream::Stdout, |t| t.yellow()).to_string(),
            Some(Level::Info) => level.if_supports_color(Stream::Stdout, |t| t.green()).to_string(),
            Some(Level::Debug) | Some(Level::Trace) => level.if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string(),
            None => level.to_string(),
        }
    } else {
        level.to_string()
    };
    let mut out = format!(
        "{} {:<5} {} {}",
        l.time,
        level_text,
        if color { l.target.if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string() } else { l.target.clone() },
        l.message
    );
    for (k, v) in &l.fields {
        out.push(' ');
        out.push_str(&format!("{k}={v}"));
    }
    out.trim_end().to_string()
}

/// Detect the level of a plain-text log line (`... INFO ...`).
pub fn text_line_level(line: &str) -> Option<Level> {
    line.split_whitespace().take(4).find_map(|tok| match tok {
        "TRACE" => Some(Level::Trace),
        "DEBUG" => Some(Level::Debug),
        "INFO" => Some(Level::Info),
        "WARN" => Some(Level::Warn),
        "ERROR" => Some(Level::Error),
        _ => None,
    })
}

/// Filters applied to each line.
#[derive(Debug, Clone, Default)]
pub struct LineFilter {
    pub min_level: Option<Level>,
    /// Any of these substrings must appear in the raw line (task key, id, short id).
    pub task_needles: Vec<String>,
}

impl LineFilter {
    /// Apply the filter and render the line (coloured when `color`). `None` = hidden.
    pub fn render(&self, raw: &str, color: bool) -> Option<String> {
        if raw.trim().is_empty() {
            return None;
        }
        if !self.task_needles.is_empty() {
            let lower = raw.to_ascii_lowercase();
            if !self.task_needles.iter().any(|n| lower.contains(&n.to_ascii_lowercase())) {
                return None;
            }
        }
        match parse_json_line(raw) {
            Some(parsed) => {
                if let Some(min) = self.min_level
                    && parsed.level.map(|l| l < min).unwrap_or(false)
                {
                    return None;
                }
                Some(render(&parsed, color))
            }
            None => {
                if let Some(min) = self.min_level
                    && text_line_level(raw).map(|l| l < min).unwrap_or(false)
                {
                    return None;
                }
                Some(raw.trim_end().to_string())
            }
        }
    }
}

/// Last `n` matching lines across files (newest file first in `files`).
pub fn tail_lines(files: &[PathBuf], n: usize, filter: &LineFilter, color: bool) -> Result<Vec<String>> {
    let mut out: VecDeque<String> = VecDeque::with_capacity(n);
    for file in files {
        let text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
        let mut from_this: Vec<String> = Vec::new();
        for raw in text.lines().rev() {
            if let Some(line) = filter.render(raw, color) {
                from_this.push(line);
                if from_this.len() + out.len() >= n {
                    break;
                }
            }
        }
        for line in from_this {
            out.push_front(line);
        }
        if out.len() >= n {
            break;
        }
    }
    Ok(out.into_iter().collect())
}

fn newest(dir: &Path) -> Option<PathBuf> {
    log_files(dir).into_iter().next()
}

/// Poll the newest log file every 500 ms and print new lines, switching to a
/// new daily file when one appears. Runs until the process is interrupted.
fn follow(dir: &Path, filter: &LineFilter, color: bool) -> Result<()> {
    let mut current: Option<PathBuf> = newest(dir);
    let mut offset: u64 = match &current {
        Some(p) => std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
        None => 0,
    };
    loop {
        let latest = newest(dir);
        if latest != current {
            current = latest;
            offset = 0;
        }
        if let Some(path) = &current
            && let Ok(mut f) = std::fs::File::open(path)
        {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < offset {
                offset = 0; // truncated / rotated in place
            }
            if len > offset {
                f.seek(SeekFrom::Start(offset))?;
                let mut reader = BufReader::new(f);
                let mut buf = String::new();
                loop {
                    buf.clear();
                    let n = reader.read_line(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    if !buf.ends_with('\n') {
                        break; // partial line; re-read next round
                    }
                    offset += n as u64;
                    if let Some(line) = filter.render(&buf, color) {
                        println!("{line}");
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn task_needles(ctx: &mut Context, needle: &str) -> Result<Vec<String>> {
    let mut needles = vec![needle.to_string()];
    if let Ok(store) = ctx.store()
        && let Ok(Some(task)) = store.find_task(needle)
    {
        needles.push(task.key.clone());
        needles.push(task.id.to_string());
        needles.push(task.id.short());
    }
    needles.sort();
    needles.dedup();
    Ok(needles)
}

/// Handle `powerqueue logs`.
pub fn run(ctx: &mut Context, args: LogsArgs) -> Result<i32> {
    ensure_initialised(ctx)?;
    let color = ctx.color;
    let min_level = match args.level.as_deref() {
        Some(l) => Some(Level::parse(l).ok_or_else(|| anyhow::anyhow!("unknown level `{l}` (trace|debug|info|warn|error)"))?),
        None => None,
    };

    if args.events {
        let store = ctx.store()?.clone();
        let events = match &args.task {
            Some(t) => {
                let task = super::task::find_task(&store, t)?;
                store.events_for_task(task.id, args.lines)?
            }
            None => store.recent_events(args.lines)?,
        };
        let min = min_level.unwrap_or(Level::Trace);
        let events: Vec<_> = events
            .into_iter()
            .filter(|e| {
                let lv = match e.level {
                    crate::domain::EventLevel::Debug => Level::Debug,
                    crate::domain::EventLevel::Info => Level::Info,
                    crate::domain::EventLevel::Warn => Level::Warn,
                    crate::domain::EventLevel::Error => Level::Error,
                };
                lv >= min
            })
            .collect();
        if ctx.json {
            println!("{}", serde_json::to_string_pretty(&events)?);
            return Ok(0);
        }
        if events.is_empty() {
            println!("{}", "no events".if_supports_color(Stream::Stdout, |t| t.dimmed()));
        }
        for e in &events {
            let task = e.task_id.map(|t| t.short()).unwrap_or_else(|| "-".to_string());
            println!(
                "{} {}",
                super::task::format_event(e, color),
                if color {
                    format!("task={task}").if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string()
                } else {
                    format!("task={task}")
                }
            );
        }
        if args.follow {
            let mut last_id = events.last().map(|e| e.id).unwrap_or(0);
            loop {
                std::thread::sleep(Duration::from_millis(500));
                for e in store.events_after(last_id, 200)? {
                    last_id = e.id;
                    println!("{}", super::task::format_event(&e, color));
                }
            }
        }
        return Ok(0);
    }

    let task_needles = match &args.task {
        Some(t) => task_needles(ctx, t)?,
        None => Vec::new(),
    };
    let filter = LineFilter { min_level, task_needles };
    let dir = ctx.paths.logs_dir();
    let files = log_files(&dir);
    if files.is_empty() {
        if args.follow {
            println!(
                "{}",
                format!("no log files in {} yet; waiting…", dir.display()).if_supports_color(Stream::Stdout, |t| t.dimmed())
            );
        } else {
            bail!("no log files in {} yet (start the daemon with `powerqueue run`)", dir.display());
        }
    }
    for line in tail_lines(&files, args.lines, &filter, color)? {
        println!("{line}");
    }
    if args.follow {
        follow(&dir, &filter, color)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{"timestamp":"2026-03-01T10:20:30.123Z","level":"INFO","fields":{"message":"session launched","task":"ENG-1","model":"opus"},"target":"powerqueue::scheduler"}"#;

    #[test]
    fn formats_json_lines() {
        let s = format_json_line(JSON).unwrap();
        assert!(s.contains("INFO"), "{s}");
        assert!(s.contains("powerqueue::scheduler session launched model=opus task=ENG-1"), "{s}");
        assert!(format_json_line("plain text line").is_none());
        assert!(format_json_line("").is_none());
    }

    #[test]
    fn level_filter_applies_to_json_and_text() {
        let f = LineFilter { min_level: Some(Level::Warn), task_needles: vec![] };
        assert!(f.render(JSON, false).is_none());
        assert!(f.render("2026-03-01T10:20:30Z  WARN powerqueue: careful", false).is_some());
        assert!(f.render("2026-03-01T10:20:30Z  DEBUG powerqueue: noisy", false).is_none());
        assert!(f.render("no level here", false).is_some());
    }

    #[test]
    fn task_filter_matches_substrings() {
        let f = LineFilter { min_level: None, task_needles: vec!["eng-1".into()] };
        assert!(f.render(JSON, false).is_some());
        assert!(f.render(r#"{"timestamp":"x","level":"INFO","fields":{"message":"other"},"target":"t"}"#, false).is_none());
    }

    #[test]
    fn tail_reads_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("powerqueue.log.2026-01-01");
        let new = dir.path().join("powerqueue.log.2026-01-02");
        std::fs::write(&old, "a\nb\n").unwrap();
        std::fs::write(&new, "c\nd\n").unwrap();
        let files = log_files(dir.path());
        let lines = tail_lines(&files, 3, &LineFilter::default(), false).unwrap();
        assert_eq!(lines, vec!["b", "c", "d"]);
        let lines = tail_lines(&files, 10, &LineFilter::default(), false).unwrap();
        assert_eq!(lines, vec!["a", "b", "c", "d"]);
    }
}
