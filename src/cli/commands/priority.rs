//! `powerqueue priority ...`: show, check, edit and explain `PRIORITY.md`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use owo_colors::{OwoColorize, Stream};

use crate::cli::output::{criticality_colored, model_colored};
use crate::cli::{Context, PriorityCommand, TaskRef};
use crate::priority::{PriorityRules, RuleError};

/// Dispatch a `priority` subcommand. Returns the exit code.
pub fn run(ctx: &mut Context, cmd: PriorityCommand) -> Result<i32> {
    match cmd {
        PriorityCommand::Show => show(ctx),
        PriorityCommand::Check => check(ctx),
        PriorityCommand::Edit => edit(ctx),
        PriorityCommand::Explain(task) => explain(ctx, &task),
        PriorityCommand::Path => {
            println!("{}", rules_path(ctx)?.display());
            Ok(0)
        }
    }
}

/// Resolved location of `PRIORITY.md` (config or default).
fn rules_path(ctx: &mut Context) -> Result<PathBuf> {
    let paths = ctx.paths.clone();
    Ok(ctx.config_or_default()?.priority_file(&paths))
}

fn missing_message(path: &Path) -> String {
    format!(
        "{} does not exist; run `powerqueue init` to create it (or `powerqueue priority edit` to start from the template).",
        path.display()
    )
}

/// Parse the file, printing problems. `Ok(None)` when it does not exist or has errors.
fn parse_file(ctx: &mut Context, path: &Path) -> Result<Option<PriorityRules>> {
    if !path.exists() {
        if ctx.json {
            println!("{}", serde_json::json!({ "ok": false, "path": path, "error": "missing", "hint": "run `powerqueue init`" }));
        } else {
            println!(
                "{} {}",
                "missing:".if_supports_color(Stream::Stdout, |t| t.style(owo_colors::Style::new().yellow().bold())),
                missing_message(path)
            );
        }
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    match PriorityRules::parse(&text) {
        Ok(rules) => Ok(Some(rules)),
        Err(errors) => {
            if ctx.json {
                println!("{}", serde_json::json!({ "ok": false, "path": path, "errors": errors }));
            } else {
                print_problems(&errors, "error");
            }
            Ok(None)
        }
    }
}

fn print_problems(problems: &[RuleError], level: &str) {
    for p in problems {
        let tag = format!("{level}:");
        let tag = if level == "error" {
            tag.red().if_supports_color(Stream::Stdout, |t| t.bold()).to_string()
        } else {
            tag.yellow().if_supports_color(Stream::Stdout, |t| t.bold()).to_string()
        };
        println!("{tag} line {}: {}", p.line, p.message);
    }
}

fn show(ctx: &mut Context) -> Result<i32> {
    let path = rules_path(ctx)?;
    let Some(rules) = parse_file(ctx, &path)? else {
        return Ok(if path.exists() { 1 } else { 0 });
    };
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "ok": true, "path": path, "rules": rules }))?);
        return Ok(0);
    }
    println!("{} {}", "Rules from".if_supports_color(Stream::Stdout, |t| t.bold()), path.display());
    print!("{}", rules.describe());
    Ok(0)
}

fn check(ctx: &mut Context) -> Result<i32> {
    let path = rules_path(ctx)?;
    let Some(rules) = parse_file(ctx, &path)? else {
        return Ok(1);
    };
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true, "path": path, "rules": rules.rule_count(), "scoring": rules.scoring.len(),
                "overrides": rules.overrides.len(), "warnings": rules.warnings,
            }))?
        );
        return Ok(0);
    }
    print_problems(&rules.warnings, "warning");
    println!(
        "{} {}: {} rule(s), {} scoring rule(s), {} override(s), {} warning(s)",
        "ok:".if_supports_color(Stream::Stdout, |t| t.style(owo_colors::Style::new().green().bold())),
        path.display(),
        rules.rule_count(),
        rules.scoring.len(),
        rules.overrides.len(),
        rules.warnings.len()
    );
    Ok(0)
}

fn edit(ctx: &mut Context) -> Result<i32> {
    let path = rules_path(ctx)?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&path, crate::priority::template()).with_context(|| format!("write template to {}", path.display()))?;
        println!("Created {} from the template.", path.display());
    }
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "vi".to_string());
    let parts = shlex::split(&editor).ok_or_else(|| anyhow!("cannot parse $EDITOR value `{editor}`"))?;
    let (program, args) = parts.split_first().ok_or_else(|| anyhow!("$EDITOR is empty"))?;
    let status = std::process::Command::new(program)
        .args(args)
        .arg(&path)
        .status()
        .with_context(|| format!("launch editor `{editor}`"))?;
    if !status.success() {
        bail!("editor `{editor}` exited with {status}");
    }
    check(ctx)
}

fn explain(ctx: &mut Context, task_ref: &TaskRef) -> Result<i32> {
    let path = rules_path(ctx)?;
    let cfg = ctx.config_or_default()?.priority.clone();
    let store = ctx.store()?.clone();
    let task = store.find_task(&task_ref.task)?.ok_or_else(|| anyhow!("no task matches `{}`", task_ref.task))?;
    let rules = if path.exists() {
        PriorityRules::load(&path)?
    } else {
        println!(
            "{} {}",
            "note:".if_supports_color(Stream::Stdout, |t| t.style(owo_colors::Style::new().yellow().bold())),
            missing_message(&path)
        );
        PriorityRules::default()
    };
    let jev_normalized = if rules.jev.enabled && cfg.jev.enabled {
        store.jev_cached(task.id)?.map(|c| {
            let n = rules.jev.levels.len().max(2) as f64;
            (c.score / (n - 1.0)).clamp(0.0, 1.0)
        })
    } else {
        None
    };
    let eval = rules.evaluate(&task, chrono::Utc::now(), jev_normalized, cfg.jev.weight, cfg.age_boost_per_hour);
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "task": task.key, "id": task.id, "rules": path, "evaluation": eval,
                "stored": { "criticality": task.criticality, "score": task.score, "reasons": task.score_reasons },
            }))?
        );
        return Ok(0);
    }
    println!("{} {} — {}", "Task".if_supports_color(Stream::Stdout, |t| t.bold()), task.key, task.title);
    println!("  rules:       {}", path.display());
    println!("  criticality: {}", criticality_colored(eval.criticality));
    println!("  score:       {:.1}", eval.score);
    println!("  model:       {}", model_colored(eval.model.as_ref()));
    if eval.skip {
        println!("  skip:        {}", "yes (override)".if_supports_color(Stream::Stdout, |t| t.red()));
    }
    println!("  reasons:");
    for r in &eval.reasons {
        println!("    - {r}");
    }
    if (task.score - eval.score).abs() > 0.5 || task.criticality != eval.criticality {
        println!(
            "  {} stored score is {:.1} ({}); the daemon re-scores on its next tick.",
            "note:".if_supports_color(Stream::Stdout, |t| t.dimmed()),
            task.score,
            task.criticality
        );
    }
    Ok(0)
}
