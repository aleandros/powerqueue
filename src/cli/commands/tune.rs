//! `powerqueue tune`: say what you expect, let Claude draft the change,
//! review the diff and the simulated queue, apply (or `--apply` / `--undo`
//! later). The mechanics live in [`crate::tune`]; this file is the flow and
//! the output.

use std::io::IsTerminal as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use dialoguer::Confirm;
use dialoguer::theme::{ColorfulTheme, SimpleTheme, Theme};
use owo_colors::{OwoColorize, Stream, Style};

use crate::budget::Ledgers;
use crate::cli::commands::config::apply_note;
use crate::cli::commands::priority::{Simulation, render_simulation, simulate_with};
use crate::cli::commands::status::ensure_initialised;
use crate::cli::output::{human_duration, truncate};
use crate::cli::{Context, SimulateArgs, TuneArgs};
use crate::config::{Config, REPO_CONFIG_FILE};
use crate::domain::{EventLevel, ModelTier, Provider, Task};
use crate::priority::PriorityRules;
use crate::tune::{
    AgentRun, CONTEXT_FILE, Draft, DraftStatus, EXIT_PROPOSED, FileChange, RESULT_FILE, agent_env, build_prompt, changes,
    claude_argv, new_draft_id, run_agent, validate,
};

/// Dispatch `powerqueue tune`. Returns the exit code: 0 applied / unchanged /
/// dry run, 1 the agent failed or produced invalid files (or the user
/// declined), [`EXIT_PROPOSED`] when a proposal is waiting for `--apply`.
pub fn run(ctx: &mut Context, args: TuneArgs) -> Result<i32> {
    ensure_initialised(ctx)?;
    if args.undo {
        return undo(ctx);
    }
    if let Some(which) = args.apply.clone() {
        return apply_existing(ctx, &args, &which);
    }
    tune(ctx, args)
}

/// Produce a new proposal.
fn tune(ctx: &mut Context, args: TuneArgs) -> Result<i32> {
    let instruction = read_instruction(args.instruction.as_deref())?;
    let cfg = ctx.config()?.clone();
    let paths = ctx.paths.clone();

    let model = match &args.model {
        Some(m) => m.clone(),
        None => cfg.tune.model.clone(),
    };
    let tier: ModelTier = model.parse().map_err(|e| anyhow!("--model: {e}"))?;
    if tier.provider() != Provider::Claude {
        bail!("`{model}` is not a Claude model; `powerqueue tune` runs on Claude Code");
    }
    let binary = cfg.claude.binary.clone();
    if let Some(t) = crate::session::BinaryTemplate::parse(&binary).ok().filter(|t| t.is_per_task()) {
        bail!(
            "claude.binary is a per-task command (`{binary}`, placeholders {}) and `powerqueue tune` runs Claude Code on this host; \
             set claude.binary to a plain command while tuning",
            t.placeholders().iter().map(|p| format!("{{{p}}}")).collect::<Vec<_>>().join(", ")
        );
    }
    if crate::session::which_program(&binary).is_none() {
        bail!(
            "`{}` not found on PATH; install Claude Code (npm install -g @anthropic-ai/claude-code) or set claude.binary",
            crate::session::program_of(&binary)
        );
    }
    let timeout = Duration::from_secs(args.timeout.unwrap_or(cfg.tune.timeout_secs).max(1));

    // Live files.
    if args.scope.includes_priority() && cfg.overrides.sets("priority.file") {
        bail!(
            "the rules file is set by the repository ({} in {}); edit it in the repository (or run with `--scope config`) \
             instead of letting tune write into the checkout",
            cfg.priority_file(&paths).display(),
            cfg.overrides.file.as_deref().map(|f| f.display().to_string()).unwrap_or_else(|| REPO_CONFIG_FILE.to_string())
        );
    }
    let live_priority = cfg.priority_file(&paths);
    let live_config = paths.config_file();
    let priority_text = match std::fs::read_to_string(&live_priority) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", live_priority.display())),
    };
    let config_text = std::fs::read_to_string(&live_config).with_context(|| format!("read {}", live_config.display()))?;
    let rules = match &priority_text {
        Some(text) => match PriorityRules::parse(text) {
            Ok(r) => r,
            Err(errors) => bail!(
                "{} has errors; fix them first (`powerqueue priority check`):\n  - {}",
                live_priority.display(),
                errors.iter().map(|e| format!("line {}: {}", e.line, e.message)).collect::<Vec<_>>().join("\n  - ")
            ),
        },
        None => PriorityRules::default(),
    };

    // Current state for the prompt.
    let sim_args = SimulateArgs { reasons: true, no_budget: args.no_budget, ..SimulateArgs::default() };
    let live_sim = simulate_with(ctx, &sim_args, &cfg, &rules, &live_priority)?;
    let tasks = ctx.store()?.list_open_tasks()?;
    let ledgers = Ledgers::load(ctx.store()?, &cfg.budget, chrono::Utc::now())?;
    let context_md = context_markdown(&cfg, &live_priority, priority_text.is_some(), &live_sim, &sim_args, &tasks, &ledgers);
    let repo_overrides = cfg.read_repo_overrides(&cfg.repo_path()).ok().flatten().map(|l| l.text);

    // Draft directory + prompt.
    let id = new_draft_id(chrono::Utc::now());
    let mut draft = Draft::create(
        &paths,
        &id,
        &instruction,
        args.scope,
        &model,
        &live_priority,
        priority_text.as_deref(),
        &live_config,
        &config_text,
    )?;
    draft.write_file(CONTEXT_FILE, &context_md)?;
    let prompt = build_prompt(&instruction, args.scope, &context_md, repo_overrides.as_deref());
    draft.write_file(crate::tune::PROMPT_FILE, &prompt)?;

    let self_bin = std::env::current_exe().context("locate the powerqueue binary")?;
    let agent = AgentRun {
        argv: claude_argv(&binary, tier.alias(), &cfg.tune.extra_args),
        cwd: draft.dir.clone(),
        env: agent_env(&paths, &self_bin),
        prompt,
        timeout,
    };
    tracing::info!(draft = %draft.dir.display(), model = %model, scope = %args.scope, "tune: starting Claude Code");
    let spinner = spinner(
        ctx,
        &format!("asking Claude ({model}) to draft the change… (timeout {})", human_duration(timeout.as_secs() as i64)),
    );
    let outcome = match run_agent(&agent) {
        Ok(o) => o,
        Err(e) => {
            if let Some(s) = &spinner {
                s.finish_and_clear();
            }
            draft.set_status(DraftStatus::Failed, vec![format!("{e:#}")])?;
            return Err(e.context(format!("tune draft kept at {}", draft.dir.display())));
        }
    };
    if let Some(s) = &spinner {
        s.finish_and_clear();
    }
    draft.write_file(RESULT_FILE, &outcome.stdout)?;
    if !outcome.stderr.trim().is_empty() {
        draft.write_file("stderr.log", &outcome.stderr)?;
    }
    draft.meta.agent = outcome.report.clone();
    draft.meta.summary = Some(outcome.result.clone()).filter(|s| !s.trim().is_empty());
    tracing::info!(
        draft = %draft.dir.display(),
        turns = ?outcome.report.num_turns,
        exit = ?outcome.report.exit_code,
        timed_out = outcome.report.timed_out,
        "tune: Claude Code finished"
    );

    if outcome.report.timed_out {
        draft.set_status(DraftStatus::Failed, vec![format!("timed out after {}", human_duration(timeout.as_secs() as i64))])?;
        return fail(
            ctx,
            &draft,
            &format!(
                "Claude did not finish within {}; raise --timeout or tune.timeout_secs",
                human_duration(timeout.as_secs() as i64)
            ),
            &outcome.stderr,
        );
    }
    if outcome.report.exit_code.is_some_and(|c| c != 0) {
        let code = outcome.report.exit_code.unwrap_or(1);
        draft.set_status(DraftStatus::Failed, vec![format!("`{binary}` exited with {code}")])?;
        return fail(ctx, &draft, &format!("`{binary}` exited with {code}"), &outcome.stderr);
    }

    // Validate and compare.
    let valid = match validate(&paths, &draft) {
        Ok(v) => v,
        Err(problems) => {
            draft.set_status(DraftStatus::Invalid, problems.clone())?;
            if ctx.json {
                print_json(&draft, &[], None, false)?;
            } else {
                print_summary(&draft);
                eprintln!(
                    "{} Claude's draft does not validate; nothing was applied ({} problem(s)):",
                    "invalid:".if_supports_color(Stream::Stderr, |t| t.style(Style::new().red().bold())),
                    problems.len()
                );
                for p in &problems {
                    eprintln!("  - {p}");
                }
                eprintln!("The draft is kept at {} for inspection.", draft.dir.display());
            }
            return Ok(1);
        }
    };
    let file_changes = changes(&draft)?;
    if !file_changes.iter().any(|c| c.changed) {
        draft.set_status(DraftStatus::Unchanged, vec![])?;
        prune(ctx, &cfg);
        if ctx.json {
            print_json(&draft, &file_changes, None, false)?;
        } else {
            print_summary(&draft);
            println!(
                "{} Claude changed nothing (see its explanation above). Draft kept at {}.",
                "no change:".if_supports_color(Stream::Stdout, |t| t.style(Style::new().yellow().bold())),
                draft.dir.display()
            );
        }
        return Ok(0);
    }
    draft.set_status(DraftStatus::Proposed, vec![])?;

    let draft_sim = simulate_with(ctx, &draft_sim_args(&args), &valid.config, &valid.rules, &draft.priority_path())?;
    review_and_apply(ctx, &cfg, &mut draft, &file_changes, &draft_sim, &args, true)
}

/// `--apply [DIR]`: validate and apply an existing proposal.
fn apply_existing(ctx: &mut Context, args: &TuneArgs, which: &str) -> Result<i32> {
    let cfg = ctx.config()?.clone();
    let paths = ctx.paths.clone();
    let mut draft = Draft::resolve(&paths, which)?;
    match draft.meta.status {
        DraftStatus::Proposed => {}
        DraftStatus::Applied => bail!("draft {} was already applied (use --undo to revert it)", draft.dir.display()),
        other => bail!("draft {} is {}; only proposed drafts can be applied", draft.dir.display(), other),
    }
    let valid = match validate(&paths, &draft) {
        Ok(v) => v,
        Err(problems) => bail!("draft {} does not validate:\n  - {}", draft.dir.display(), problems.join("\n  - ")),
    };
    let file_changes = changes(&draft)?;
    if !file_changes.iter().any(|c| c.changed) {
        draft.set_status(DraftStatus::Unchanged, vec![])?;
        if ctx.json {
            print_json(&draft, &file_changes, None, false)?;
        } else {
            println!("the live files already match the draft; nothing to apply");
        }
        return Ok(0);
    }
    let draft_sim = simulate_with(ctx, &draft_sim_args(args), &valid.config, &valid.rules, &draft.priority_path())?;
    review_and_apply(ctx, &cfg, &mut draft, &file_changes, &draft_sim, args, false)
}

/// Show the proposal and apply it according to `--yes` / `--dry-run` / the terminal.
fn review_and_apply(
    ctx: &mut Context,
    cfg: &Config,
    draft: &mut Draft,
    file_changes: &[FileChange],
    draft_sim: &Simulation,
    args: &TuneArgs,
    fresh: bool,
) -> Result<i32> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let decision = if args.dry_run {
        Decision::DryRun
    } else if args.yes {
        Decision::Apply
    } else if interactive && !ctx.json {
        Decision::Ask
    } else {
        Decision::Proposed
    };

    if !ctx.json {
        if fresh {
            print_summary(draft);
        } else {
            println!(
                "{} {} ({}, {})",
                "Draft".if_supports_color(Stream::Stdout, |t| t.bold()),
                draft.dir.display(),
                draft.meta.created_at.format("%Y-%m-%d %H:%M UTC"),
                truncate(&draft.meta.instruction, 60)
            );
            print_summary(draft);
        }
        print_diffs(file_changes);
        println!();
        print!("{}", render_simulation(draft_sim, &draft_sim_args(args)));
        println!();
    }

    let apply_now = match decision {
        Decision::DryRun => false,
        Decision::Apply => true,
        Decision::Proposed => false,
        Decision::Ask => {
            let theme: Box<dyn Theme> = if ctx.color { Box::new(ColorfulTheme::default()) } else { Box::new(SimpleTheme) };
            let names: Vec<&str> = file_changes.iter().filter(|c| c.changed).map(|c| c.name.as_str()).collect();
            Confirm::with_theme(theme.as_ref())
                .with_prompt(format!("Apply the change to {}?", names.join(" and ")))
                .default(true)
                .interact()
                .context("read confirmation")?
        }
    };

    if !apply_now {
        if ctx.json {
            print_json(draft, file_changes, Some(draft_sim), false)?;
        } else {
            let how = match decision {
                Decision::DryRun => "dry run: nothing was applied.",
                Decision::Ask => "not applied.",
                _ => "not applied (no terminal to ask on; pass --yes to apply directly).",
            };
            println!(
                "{} {how} Draft kept at {}; apply it later with `powerqueue tune --apply`.",
                "proposed:".if_supports_color(Stream::Stdout, |t| t.style(Style::new().yellow().bold())),
                draft.dir.display()
            );
        }
        return Ok(match decision {
            Decision::Ask => 1,
            Decision::DryRun => 0,
            _ => EXIT_PROPOSED,
        });
    }

    let written = crate::tune::apply(draft, file_changes)?;
    let store = ctx.store()?.clone();
    store.log_event(
        None,
        None,
        EventLevel::Info,
        "tune.applied",
        &format!("tune: applied `{}` to {}", truncate(&draft.meta.instruction, 80), names_of(file_changes)),
        serde_json::json!({ "draft": draft.dir, "files": written, "model": draft.meta.model }),
    )?;
    tracing::info!(draft = %draft.dir.display(), files = ?written, "tune: applied");
    let note = apply_note(ctx)?;
    prune(ctx, cfg);
    if ctx.json {
        print_json(draft, file_changes, Some(draft_sim), true)?;
    } else {
        println!(
            "{} {} ({}). Revert with `powerqueue tune --undo`.",
            "applied:".if_supports_color(Stream::Stdout, |t| t.style(Style::new().green().bold())),
            written.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
            note
        );
    }
    Ok(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    DryRun,
    Apply,
    Ask,
    Proposed,
}

/// `--undo`: restore the files of the most recent applied draft.
fn undo(ctx: &mut Context) -> Result<i32> {
    let paths = ctx.paths.clone();
    let Some(mut draft) = Draft::latest_with(&paths, DraftStatus::Applied)? else {
        bail!("no applied tune draft to undo");
    };
    let touched = crate::tune::undo(&mut draft)?;
    let store = ctx.store()?.clone();
    store.log_event(
        None,
        None,
        EventLevel::Info,
        "tune.undone",
        &format!("tune: reverted `{}`", truncate(&draft.meta.instruction, 80)),
        serde_json::json!({ "draft": draft.dir, "files": touched }),
    )?;
    tracing::info!(draft = %draft.dir.display(), files = ?touched, "tune: undone");
    let note = apply_note(ctx)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "draft": draft.dir, "status": draft.meta.status, "instruction": draft.meta.instruction,
                "restored": touched, "daemon": note,
            }))?
        );
    } else {
        println!(
            "{} restored {} from {} ({})",
            "undone:".if_supports_color(Stream::Stdout, |t| t.style(Style::new().green().bold())),
            touched.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
            draft.dir.display(),
            note
        );
    }
    Ok(0)
}

// ------------------------------------------------------------------ helpers

fn read_instruction(arg: Option<&str>) -> Result<String> {
    let text = match arg {
        Some("-") | None => {
            if std::io::stdin().is_terminal() {
                bail!(
                    "say what you expect, e.g. `powerqueue tune \"ENG-12 should run before ENG-40\"` (or pipe the text on stdin)"
                );
            }
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).context("read the instruction from stdin")?;
            buf
        }
        Some(s) => s.to_string(),
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        bail!("the instruction is empty");
    }
    Ok(text)
}

fn draft_sim_args(args: &TuneArgs) -> SimulateArgs {
    SimulateArgs { file: Some("draft".into()), no_budget: args.no_budget, ..SimulateArgs::default() }
}

fn spinner(ctx: &Context, msg: &str) -> Option<indicatif::ProgressBar> {
    if ctx.json || !std::io::stderr().is_terminal() || matches!(ctx.verbosity, crate::logging::Verbosity::Quiet) {
        if !ctx.json && !matches!(ctx.verbosity, crate::logging::Verbosity::Quiet) {
            eprintln!("{msg}");
        }
        return None;
    }
    let pb = indicatif::ProgressBar::new_spinner().with_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    Some(pb)
}

fn fail(ctx: &mut Context, draft: &Draft, what: &str, stderr: &str) -> Result<i32> {
    if ctx.json {
        print_json(draft, &[], None, false)?;
        return Ok(1);
    }
    print_summary(draft);
    eprintln!(
        "{} {what}; nothing was applied",
        "failed:".if_supports_color(Stream::Stderr, |t| t.style(Style::new().red().bold()))
    );
    let tail: Vec<&str> = stderr.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    if !tail.is_empty() {
        eprintln!("  {}:", "claude stderr".if_supports_color(Stream::Stderr, |t| t.dimmed()));
        for line in tail {
            eprintln!("    {line}");
        }
    }
    eprintln!("The draft is kept at {}.", draft.dir.display());
    Ok(1)
}

fn names_of(changes: &[FileChange]) -> String {
    changes.iter().filter(|c| c.changed).map(|c| c.name.as_str()).collect::<Vec<_>>().join(" and ")
}

fn print_summary(draft: &Draft) {
    let Some(summary) = draft.meta.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    println!("{}", "Claude:".if_supports_color(Stream::Stdout, |t| t.bold()));
    for line in summary.lines() {
        println!("  {line}");
    }
    let r = &draft.meta.agent;
    let mut bits = Vec::new();
    if let Some(t) = r.num_turns {
        bits.push(format!("{t} turn(s)"));
    }
    if let Some(ms) = r.duration_ms {
        bits.push(human_duration((ms / 1000) as i64));
    }
    if r.permission_denials > 0 {
        bits.push(format!("{} tool call(s) refused", r.permission_denials));
    }
    if !bits.is_empty() {
        println!("  {}", format!("[{} {}]", draft.meta.model, bits.join(", ")).if_supports_color(Stream::Stdout, |t| t.dimmed()));
    }
    println!();
}

fn print_diffs(changes: &[FileChange]) {
    for c in changes {
        if !c.changed {
            println!("{} {} unchanged", "·".if_supports_color(Stream::Stdout, |t| t.dimmed()), c.name);
            continue;
        }
        println!("{} {} → {}", "Change to".if_supports_color(Stream::Stdout, |t| t.bold()), c.name, c.live.display());
        if c.live_changed_since_draft {
            println!(
                "  {} the live file changed after this draft was made; applying replaces the current content",
                "warning:".if_supports_color(Stream::Stdout, |t| t.style(Style::new().yellow().bold()))
            );
        }
        if c.before.is_none() {
            println!("  {}", "(the live file does not exist yet)".if_supports_color(Stream::Stdout, |t| t.dimmed()));
        }
        for line in c.diff.lines() {
            let painted = if line.starts_with("+++") || line.starts_with("---") {
                line.if_supports_color(Stream::Stdout, |t| t.bold()).to_string()
            } else if line.starts_with('+') {
                line.if_supports_color(Stream::Stdout, |t| t.green()).to_string()
            } else if line.starts_with('-') {
                line.if_supports_color(Stream::Stdout, |t| t.red()).to_string()
            } else if line.starts_with("@@") {
                line.if_supports_color(Stream::Stdout, |t| t.cyan()).to_string()
            } else {
                line.to_string()
            };
            println!("  {painted}");
        }
    }
}

fn print_json(draft: &Draft, changes: &[FileChange], sim: Option<&Simulation>, applied: bool) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "draft": draft.dir,
            "id": draft.meta.id,
            "status": draft.meta.status,
            "instruction": draft.meta.instruction,
            "scope": draft.meta.scope,
            "model": draft.meta.model,
            "summary": draft.meta.summary,
            "agent": draft.meta.agent,
            "problems": draft.meta.problems,
            "files": changes,
            "simulation": sim,
            "applied": applied,
        }))?
    );
    Ok(())
}

fn prune(ctx: &mut Context, cfg: &Config) {
    match crate::tune::prune(&ctx.paths, cfg.tune.keep_drafts) {
        Ok(n) if n > 0 => tracing::debug!(removed = n, "tune: pruned old drafts"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "tune: could not prune old drafts"),
    }
}

/// `CONTEXT.md`: the current queue (simulated with the live rules), the
/// open tasks with every field the rules can match, and the budget state.
pub fn context_markdown(
    cfg: &Config,
    live_priority: &Path,
    priority_exists: bool,
    sim: &Simulation,
    sim_args: &SimulateArgs,
    tasks: &[Task],
    ledgers: &Ledgers,
) -> String {
    use std::fmt::Write as _;
    let mut md = String::new();
    let _ = writeln!(md, "### Queue right now (`powerqueue priority simulate --reasons` with the live rules)\n");
    if !priority_exists {
        let _ = writeln!(
            md,
            "_The live `{}` does not exist yet; the draft `PRIORITY.md` starts from the template and every task is `normal`._\n",
            live_priority.display()
        );
    }
    if sim.rows.is_empty() {
        let _ = writeln!(md, "_The queue is empty: no open tasks to rank._\n");
    } else {
        let _ = writeln!(md, "```text");
        let rendered = render_simulation(sim, sim_args);
        md.push_str(strip_ansi(&rendered).trim_end());
        let _ = writeln!(md, "\n```\n");
    }

    let _ = writeln!(md, "### Open tasks (fields the rules can match)\n");
    if tasks.is_empty() {
        let _ = writeln!(md, "_none_\n");
    } else {
        let _ = writeln!(
            md,
            "| key | state | criticality | score | labels | linear priority | estimate | project | cycle | source | age | title |"
        );
        let _ = writeln!(md, "|---|---|---|---|---|---|---|---|---|---|---|---|");
        let now = chrono::Utc::now();
        for t in tasks {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {:.0} | {} | {} | {} | {} | {} | {} | {} | {} |",
                t.key,
                t.state,
                t.criticality,
                t.score,
                if t.labels.is_empty() { "–".to_string() } else { t.labels.join(", ") },
                t.linear_priority.map(|p| p.to_string()).unwrap_or_else(|| "–".into()),
                t.estimate.map(|e| format!("{e}")).unwrap_or_else(|| "–".into()),
                t.project.as_deref().unwrap_or("–"),
                match (&t.cycle, t.cycle_number) {
                    (Some(c), Some(n)) => format!("{c} #{n}"),
                    (Some(c), None) => c.clone(),
                    _ => "–".to_string(),
                },
                t.source.kind(),
                human_duration((now - t.created_at).num_seconds()),
                truncate(&t.title.replace('|', "\\|"), 60),
            );
        }
        md.push('\n');
        let with_desc: Vec<&Task> = tasks.iter().filter(|t| !t.description.trim().is_empty()).collect();
        if !with_desc.is_empty() {
            let _ = writeln!(md, "Descriptions (first lines):\n");
            for t in with_desc {
                let first: String =
                    t.description.lines().map(str::trim).filter(|l| !l.is_empty()).take(3).collect::<Vec<_>>().join(" ");
                let _ = writeln!(md, "- **{}**: {}", t.key, truncate(&first, 240));
            }
            md.push('\n');
        }
    }

    let _ = writeln!(md, "### Budget state\n");
    let _ = writeln!(md, "- scheduler.max_concurrent = {}", cfg.scheduler.max_concurrent);
    for (provider, ledger) in ledgers.iter() {
        let pb = cfg.budget.provider(provider);
        let period_pct =
            if ledger.period_budget > 0.0 { ledger.total_period_weighted / ledger.period_budget * 100.0 } else { 0.0 };
        let elapsed_pct = ledger.period.elapsed_fraction(ledger.now) * 100.0;
        let _ = write!(
            md,
            "- **{provider}**: period {} → {} ({elapsed_pct:.0}% elapsed, {period_pct:.0}% of the weighted budget spent",
            ledger.period.start.format("%Y-%m-%d %H:%M"),
            ledger.period.end.format("%Y-%m-%d %H:%M"),
        );
        if ledger.has_window() {
            let _ = write!(md, ", window {:.0}% used ({})", ledger.window_fraction() * 100.0, ledger.window_fraction_source());
        }
        match ledger.learned.period {
            Some(r) => {
                let _ = write!(
                    md,
                    ", learned rate: 100% of the period ≈ {:.0} weighted tokens (configured {:.0})",
                    r.budget, ledger.configured_period_budget
                );
            }
            None => {
                let _ = write!(
                    md,
                    ", rate not learned yet (configured {:.0} weighted tokens per period)",
                    ledger.configured_period_budget
                );
            }
        }
        if let Some(obs) = &ledger.observed {
            if let Some(p) = obs.period_used {
                let _ = write!(md, ", provider reports {:.0}% of the period used", p * 100.0);
            }
            if obs.blocked {
                let _ = write!(md, ", **blocked by the provider right now**");
            }
        }
        let _ = writeln!(md, ")");
        for tier in &ledger.tiers {
            let share = pb.models.get(&tier.tier).map(|m| m.share).unwrap_or(0.0);
            let _ = writeln!(
                md,
                "  - {}: share {:.0}%, {:.0}% of its share spent, {} message(s) this period",
                tier.tier,
                share * 100.0,
                tier.period_spent_fraction() * 100.0,
                tier.messages
            );
        }
    }
    md.push('\n');
    md
}

/// Remove ANSI escape sequences (the simulation table may be coloured).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::priority::SimRow;
    use crate::domain::{Criticality, TaskSource, TaskState};

    #[test]
    fn strip_ansi_removes_colours() {
        assert_eq!(strip_ansi("\u{1b}[1mbold\u{1b}[0m plain"), "bold plain");
        assert_eq!(strip_ansi("none"), "none");
    }

    #[test]
    fn context_markdown_lists_tasks_and_budget() {
        let mut cfg = Config::default();
        cfg.scheduler.max_concurrent = 2;
        let mut t = Task::new("ENG-1", "Fix | the thing", TaskSource::Manual);
        t.labels = vec!["bug".into()];
        t.description = "line one\n\nline two".into();
        let sim = Simulation {
            rules: "/x/PRIORITY.md".into(),
            warnings: vec![],
            max_concurrent: 2,
            draft: false,
            rows: vec![SimRow {
                rank: 1,
                key: "ENG-1".into(),
                title: "Fix | the thing".into(),
                state: TaskState::Queued,
                new: false,
                schedulable: true,
                stored_rank: Some(1),
                stored_criticality: Criticality::Normal,
                stored_score: 100.0,
                criticality: Criticality::High,
                score: 300.0,
                skip: false,
                preferred_models: vec![],
                model_source: None,
                model: None,
                policy: String::new(),
                would_start_now: true,
                reasons: vec!["matched rule at line 2".into()],
            }],
        };
        let store = crate::store::Store::open_in_memory().unwrap();
        let ledgers = Ledgers::load(&store, &cfg.budget, chrono::Utc::now()).unwrap();
        let args = SimulateArgs { reasons: true, no_budget: true, ..SimulateArgs::default() };
        let md = context_markdown(&cfg, Path::new("/x/PRIORITY.md"), false, &sim, &args, &[t], &ledgers);
        assert!(md.contains("does not exist yet"));
        assert!(md.contains("| ENG-1 | queued | normal | 100 | bug |"));
        assert!(md.contains("Fix \\| the thing"));
        assert!(md.contains("- **ENG-1**: line one line two"));
        assert!(md.contains("matched rule at line 2"));
        assert!(md.contains("- scheduler.max_concurrent = 2"));
        assert!(md.contains("- **claude**: period"));
        assert!(md.contains("fable: share"));
        let empty = context_markdown(&cfg, Path::new("/x"), true, &Simulation { rows: vec![], ..sim }, &args, &[], &ledgers);
        assert!(empty.contains("The queue is empty"));
        assert!(empty.contains("_none_"));
    }

    #[test]
    fn instruction_comes_from_the_argument() {
        assert_eq!(read_instruction(Some("  make it so ")).unwrap(), "make it so");
        assert!(read_instruction(Some("   ")).is_err());
    }
}
