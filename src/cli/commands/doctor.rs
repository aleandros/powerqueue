//! `powerqueue doctor` — render diagnostics grouped by category.

use anyhow::Result;
use owo_colors::{OwoColorize, Stream, Style};

use crate::cli::{Context, DoctorArgs};
use crate::doctor::{CATEGORIES, CheckResult, Status, exit_code, run_all};

use super::status::ensure_initialised;

/// Symbol for a status: `✓` ok, `!` warn, `✗` fail, `–` skipped.
pub fn symbol(status: Status) -> &'static str {
    match status {
        Status::Ok => "✓",
        Status::Warn => "!",
        Status::Fail => "✗",
        Status::Skipped => "–",
    }
}

fn symbol_colored(status: Status, color: bool) -> String {
    let s = symbol(status);
    if !color {
        return s.to_string();
    }
    match status {
        Status::Ok => s.if_supports_color(Stream::Stdout, |t| t.green()).to_string(),
        Status::Warn => s.if_supports_color(Stream::Stdout, |t| t.style(Style::new().yellow().bold())).to_string(),
        Status::Fail => s.if_supports_color(Stream::Stdout, |t| t.style(Style::new().red().bold())).to_string(),
        Status::Skipped => s.if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string(),
    }
}

/// Counts per status for the summary line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub ok: usize,
    pub warn: usize,
    pub fail: usize,
    pub skipped: usize,
    pub fixed: usize,
}

/// Tally results.
pub fn summarize(results: &[CheckResult]) -> Summary {
    let mut s = Summary::default();
    for r in results {
        match r.status {
            Status::Ok => s.ok += 1,
            Status::Warn => s.warn += 1,
            Status::Fail => s.fail += 1,
            Status::Skipped => s.skipped += 1,
        }
        if r.fixed {
            s.fixed += 1;
        }
    }
    s
}

/// Render the report as text.
pub fn render(results: &[CheckResult], color: bool) -> String {
    let mut out = String::new();
    for cat in CATEGORIES {
        let rows: Vec<_> = results.iter().filter(|r| r.category == cat).collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&if color { cat.if_supports_color(Stream::Stdout, |t| t.bold()).to_string() } else { cat.to_string() });
        out.push('\n');
        for r in rows {
            let name = if color {
                format!("{:<22}", r.name).if_supports_color(Stream::Stdout, |t| t.bold()).to_string()
            } else {
                format!("{:<22}", r.name)
            };
            let detail = if color && r.status == Status::Skipped {
                r.detail.if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string()
            } else {
                r.detail.clone()
            };
            let fixed = if r.fixed {
                if color {
                    " (fixed)".if_supports_color(Stream::Stdout, |t| t.green()).to_string()
                } else {
                    " (fixed)".to_string()
                }
            } else {
                String::new()
            };
            out.push_str(&format!("  {} {name} {detail}{fixed}\n", symbol_colored(r.status, color)));
            if let Some(h) = &r.fix_hint
                && !r.fixed
                && r.status != Status::Ok
            {
                let line = format!("{:<26}fix: {h}", "");
                out.push_str(&if color { line.if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string() } else { line });
                out.push('\n');
            }
        }
        out.push('\n');
    }
    let s = summarize(results);
    let mut parts = vec![format!("{} ok", s.ok)];
    if s.warn > 0 {
        parts.push(format!("{} warning(s)", s.warn));
    }
    if s.fail > 0 {
        parts.push(format!("{} failure(s)", s.fail));
    }
    if s.skipped > 0 {
        parts.push(format!("{} skipped", s.skipped));
    }
    if s.fixed > 0 {
        parts.push(format!("{} fixed", s.fixed));
    }
    let line = parts.join(", ");
    let line = if !color {
        line
    } else if s.fail > 0 {
        line.if_supports_color(Stream::Stdout, |t| t.style(Style::new().red().bold())).to_string()
    } else if s.warn > 0 {
        line.if_supports_color(Stream::Stdout, |t| t.yellow()).to_string()
    } else {
        line.if_supports_color(Stream::Stdout, |t| t.green()).to_string()
    };
    out.push_str(&line);
    out.push('\n');
    out
}

/// Handle `powerqueue doctor`.
pub fn run(ctx: &mut Context, args: DoctorArgs) -> Result<i32> {
    ensure_initialised(ctx)?;
    let cfg = ctx.config_cloned()?;
    let paths = ctx.paths.clone();
    let store = ctx.store()?.clone();
    let json = ctx.json;
    let color = ctx.color;
    let secrets = ctx.secrets();
    let rt = super::runtime()?;
    let results = rt.block_on(run_all(&cfg, &paths, &store, secrets, args.fix, !args.offline))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        print!("{}", render(&results, color));
    }
    Ok(exit_code(&results))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_groups_and_summarises() {
        let results = vec![
            CheckResult::ok("environment", "git", "git 2.44"),
            CheckResult::warn("state", "daemon", "not running", "start it"),
            CheckResult::fail("secrets", "linear key", "missing", "set it"),
            CheckResult::skipped("secrets", "jev key", "disabled"),
        ];
        let text = render(&results, false);
        assert!(text.contains("environment\n  ✓ git"));
        assert!(text.contains("fix: start it"));
        assert!(text.contains("fix: set it"));
        assert!(text.ends_with("1 ok, 1 warning(s), 1 failure(s), 1 skipped\n"));
        let idx_env = text.find("environment").unwrap();
        let idx_sec = text.find("secrets").unwrap();
        let idx_state = text.find("state").unwrap();
        assert!(idx_env < idx_sec && idx_sec < idx_state);
        let s = summarize(&results);
        assert_eq!(s, Summary { ok: 1, warn: 1, fail: 1, skipped: 1, fixed: 0 });
        assert_eq!(symbol(Status::Fail), "✗");
    }

    #[test]
    fn fixed_results_hide_hint() {
        let mut r = CheckResult::warn("state", "daemon lock", "stale", "remove it");
        r.fixed = true;
        let text = render(&[r], false);
        assert!(text.contains("(fixed)"));
        assert!(!text.contains("fix: remove it"));
        assert!(text.contains("1 fixed"));
    }
}
