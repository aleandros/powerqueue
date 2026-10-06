//! Terminal output helpers: colours, tables, humanised numbers.

use std::sync::atomic::{AtomicBool, Ordering};

use owo_colors::OwoColorize;

use crate::domain::{Criticality, ModelTier, Provider, TaskState};

static COLOR: AtomicBool = AtomicBool::new(true);

/// Enable or disable ANSI colours for every helper in this module.
pub fn set_color(enabled: bool) {
    COLOR.store(enabled, Ordering::Relaxed);
    owo_colors::set_override(enabled);
}

/// Whether colours are currently enabled.
pub fn color_enabled() -> bool {
    COLOR.load(Ordering::Relaxed)
}

/// Apply `style` only when colours are enabled.
pub fn paint(text: &str, style: impl Fn(&str) -> String) -> String {
    if color_enabled() { style(text) } else { text.to_string() }
}

/// Print an anyhow error chain.
pub fn print_error(err: &anyhow::Error) {
    eprintln!("{} {}", paint("error:", |t| t.red().bold().to_string()), err);
    for cause in err.chain().skip(1) {
        eprintln!("  {} {}", paint("caused by:", |t| t.dimmed().to_string()), cause);
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
    if !color_enabled() {
        return s.to_string();
    }
    match state {
        TaskState::Running => s.green().to_string(),
        TaskState::Queued | TaskState::Starting => s.cyan().to_string(),
        TaskState::Idle => s.yellow().to_string(),
        TaskState::Crashed | TaskState::Failed => s.red().to_string(),
        TaskState::Throttled | TaskState::Paused => s.magenta().to_string(),
        TaskState::Blocked => s.blue().to_string(),
        TaskState::NeedsAttention => s.bright_yellow().bold().to_string(),
        TaskState::Completed => s.bright_green().to_string(),
        TaskState::Cancelled => s.dimmed().to_string(),
    }
}

pub fn criticality_colored(c: Criticality) -> String {
    if !color_enabled() {
        return c.as_str().to_string();
    }
    match c {
        Criticality::Critical => c.as_str().red().bold().to_string(),
        Criticality::High => c.as_str().yellow().to_string(),
        Criticality::Normal => c.as_str().to_string(),
        Criticality::Low => c.as_str().dimmed().to_string(),
    }
}

/// A model name coloured by provider and capability: Claude's tiers keep
/// their colours (fable magenta, opus blue, sonnet cyan, haiku dim), Codex
/// models are green and Gemini models yellow.
pub fn model_colored(m: Option<&ModelTier>) -> String {
    if !color_enabled() {
        return m.map(|m| m.as_str().to_string()).unwrap_or_else(|| "-".to_string());
    }
    let Some(m) = m else { return "-".dimmed().to_string() };
    let name = m.as_str();
    match (m.provider(), m.alias()) {
        (Provider::Claude, "fable") => name.magenta().bold().to_string(),
        (Provider::Claude, "opus") => name.blue().to_string(),
        (Provider::Claude, "sonnet") => name.cyan().to_string(),
        (Provider::Claude, "haiku") => name.dimmed().to_string(),
        (Provider::Claude, _) => name.to_string(),
        (Provider::Codex, _) => name.green().to_string(),
        (Provider::Gemini, _) => name.yellow().to_string(),
    }
}

/// `gpt-6-astra (codex)`: a model with its provider when it is not Claude,
/// which stays the plain alias (the common case).
pub fn model_with_provider(m: &ModelTier) -> String {
    match m.provider() {
        Provider::Claude => m.as_str().to_string(),
        p => format!("{} ({p})", m.as_str()),
    }
}

/// [`model_colored`] followed by the provider for non-Claude models.
pub fn model_colored_with_provider(m: Option<&ModelTier>) -> String {
    match m {
        Some(m) if m.provider() != Provider::Claude => {
            let suffix = format!(" ({})", m.provider());
            format!("{}{}", model_colored(Some(m)), if color_enabled() { suffix.dimmed().to_string() } else { suffix })
        }
        other => model_colored(other),
    }
}

/// A preference list (`fable | gpt-6.1-sol (codex)`), each model coloured;
/// `-` when empty.
pub fn model_list_colored(models: &[ModelTier]) -> String {
    if models.is_empty() {
        return model_colored(None);
    }
    models.iter().map(|m| model_colored_with_provider(Some(m))).collect::<Vec<_>>().join(" | ")
}

/// Print a warning line to stderr (`warning: ...`), so `--json` output on
/// stdout stays clean.
pub fn print_warning(msg: &str) {
    eprintln!("{} {msg}", paint("warning:", |t| t.yellow().bold().to_string()));
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

    #[test]
    fn models_print_their_provider_unless_claude() {
        set_color(false);
        assert_eq!(model_with_provider(&ModelTier::opus()), "opus");
        assert_eq!(model_with_provider(&ModelTier::new("gpt-6-astra")), "gpt-6-astra (codex)");
        assert_eq!(model_colored_with_provider(Some(&ModelTier::new("gemini-3-pro"))), "gemini-3-pro (gemini)");
        assert_eq!(model_colored_with_provider(None), "-");
        assert_eq!(model_list_colored(&[]), "-");
        assert_eq!(model_list_colored(&[ModelTier::fable(), ModelTier::new("gpt-6.1-sol")]), "fable | gpt-6.1-sol (codex)");
    }
}
