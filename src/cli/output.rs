//! Terminal output helpers: colours, tables, humanised numbers.

use owo_colors::OwoColorize;

use crate::domain::{Criticality, ModelTier, TaskState};

/// Print an anyhow error chain.
pub fn print_error(err: &anyhow::Error) {
    eprintln!("{} {}", "error:".red().bold(), err);
    for cause in err.chain().skip(1) {
        eprintln!("  {} {}", "caused by:".dimmed(), cause);
    }
}

/// `1.2M`, `340k`, `12`.
pub fn human_tokens(n: u64) -> String {
    human_f64(n as f64)
}

pub fn human_f64(n: f64) -> String {
    if n >= 1_000_000_000.0 {
        format!("{:.1}B", n / 1_000_000_000.0)
    } else if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.0}k", n / 1_000.0)
    } else {
        format!("{n:.0}")
    }
}

/// `1.3 GB`, `512 MB`.
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

/// `2h13m`, `45s`, `3d`.
pub fn human_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

/// Relative time such as `3m ago`.
pub fn ago(dt: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - dt).num_seconds();
    format!("{} ago", human_duration(secs))
}

/// Colour a task state consistently across status/dashboard.
pub fn state_colored(state: TaskState) -> String {
    let s = state.as_str();
    match state {
        TaskState::Running => s.green().to_string(),
        TaskState::Queued | TaskState::Starting => s.cyan().to_string(),
        TaskState::Idle => s.yellow().to_string(),
        TaskState::Crashed | TaskState::Failed => s.red().to_string(),
        TaskState::Throttled | TaskState::Paused => s.magenta().to_string(),
        TaskState::NeedsAttention => s.bright_yellow().bold().to_string(),
        TaskState::Completed => s.bright_green().to_string(),
        TaskState::Cancelled => s.dimmed().to_string(),
    }
}

pub fn criticality_colored(c: Criticality) -> String {
    match c {
        Criticality::Critical => c.as_str().red().bold().to_string(),
        Criticality::High => c.as_str().yellow().to_string(),
        Criticality::Normal => c.as_str().to_string(),
        Criticality::Low => c.as_str().dimmed().to_string(),
    }
}

pub fn model_colored(m: Option<ModelTier>) -> String {
    match m {
        Some(ModelTier::Fable) => "fable".magenta().bold().to_string(),
        Some(ModelTier::Opus) => "opus".blue().to_string(),
        Some(ModelTier::Sonnet) => "sonnet".cyan().to_string(),
        Some(ModelTier::Haiku) => "haiku".dimmed().to_string(),
        None => "-".dimmed().to_string(),
    }
}

/// Truncate to `max` columns with an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0;
    let mut out = String::new();
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if width + w > max.saturating_sub(1) {
            out.push('…');
            return out;
        }
        width += w;
        out.push(ch);
    }
    out
}

/// A standard table preset.
pub fn table() -> comfy_table::Table {
    let mut t = comfy_table::Table::new();
    *t.style_mut() = comfy_table::presets::UTF8_FULL_CONDENSED;
    t.set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanisers() {
        assert_eq!(human_tokens(999), "999");
        assert_eq!(human_tokens(12_300), "12k");
        assert_eq!(human_tokens(1_250_000), "1.2M");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(125), "2m05s");
        assert_eq!(human_duration(3_725), "1h02m");
        assert_eq!(human_duration(90_000), "1d1h");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("hi", 6), "hi");
    }
}
