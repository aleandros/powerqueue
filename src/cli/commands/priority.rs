//! `powerqueue priority ...`: show, check, edit, explain and simulate `PRIORITY.md`.
//!
//! [`simulate_with`] and [`render_simulation`] are shared with `powerqueue
//! tune`, which simulates a draft rules file against a draft config.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use owo_colors::{OwoColorize, Stream};

use chrono::{DateTime, Utc};

use crate::budget::{Estimator, Ledgers, Policy, RATE_LIMITS_KEY, RateLimitState, tier_weight};
use crate::cli::output::{criticality_colored, model_colored, model_list_colored, table, truncate};
use crate::cli::{CheckArgs, Context, PriorityCommand, SimulateArgs, TaskRef};
use crate::config::Config;
use crate::domain::{Criticality, ModelTier, Task, TaskState};
use crate::priority::{Evaluation, PriorityRules, RuleError, fmt_conditions};
use crate::scheduler::pick_next;
use crate::store::Store;

/// Dispatch a `priority` subcommand. Returns the exit code.
pub fn run(ctx: &mut Context, cmd: PriorityCommand) -> Result<i32> {
    match cmd {
        PriorityCommand::Show => show(ctx),
        PriorityCommand::Check(args) => check(ctx, &args),
        PriorityCommand::Edit => edit(ctx),
        PriorityCommand::Explain(task) => explain(ctx, &task),
        PriorityCommand::Simulate(args) => simulate(ctx, args),
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

fn check(ctx: &mut Context, args: &CheckArgs) -> Result<i32> {
    let path = match &args.file {
        Some(p) => p.clone(),
        None => rules_path(ctx)?,
    };
    if args.file.is_some() && !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let Some(rules) = parse_file(ctx, &path)? else {
        return Ok(1);
    };
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true, "path": path, "rules": rules.rule_count(), "scoring": rules.scoring.len(),
                "overrides": rules.overrides.len(), "model_rules": rules.model_rules.len(), "warnings": rules.warnings,
            }))?
        );
        return Ok(0);
    }
    print_problems(&rules.warnings, "warning");
    println!(
        "{} {}: {} rule(s), {} scoring rule(s), {} override(s), {} model if-row(s), {} warning(s)",
        "ok:".if_supports_color(Stream::Stdout, |t| t.style(owo_colors::Style::new().green().bold())),
        path.display(),
        rules.rule_count(),
        rules.scoring.len(),
        rules.overrides.len(),
        rules.model_rules.len(),
        rules.warnings.len()
    );
    print_model_lists(&rules);
    let cfg = ctx.config_or_default()?.clone();
    for r in &rules.model_rules {
        if let Some(note) = r.models.first().and_then(|m| reservation_note(&cfg, m, None)) {
            print_note(&format!("line {}: {note}", r.line));
        }
    }
    Ok(0)
}

/// `fable (if label: model/fable)`: a model list followed by the line that chose it.
fn with_model_source(models: String, source: Option<&str>) -> String {
    match source {
        Some(src) => {
            let note = format!("({src})");
            format!("{models} {}", note.if_supports_color(Stream::Stdout, |t| t.dimmed()))
        }
        None => models,
    }
}

/// A note when the budget reserves `model` for tasks more critical than
/// `task` (or, with `None`, for anything above `low`): a preference from
/// `PRIORITY.md` does not lift that reservation, so the policy runs a
/// downgrade until it relaxes late in the period.
fn reservation_note(cfg: &Config, model: &ModelTier, task: Option<Criticality>) -> Option<String> {
    let budget = cfg.budget.model_budget(model)?;
    let reserved = budget.min_criticality;
    if reserved == Criticality::Low || task.is_some_and(|c| c <= reserved) {
        return None;
    }
    let who = task.map(|c| format!("; this task is {c}")).unwrap_or_default();
    Some(format!(
        "{model} is reserved for {reserved} tasks (budget.providers.{}.models.{}.min_criticality){who}; less critical tasks get a downgrade until the reservation relaxes",
        model.provider(),
        model.alias()
    ))
}

fn print_note(msg: &str) {
    println!("{} {msg}", "note:".if_supports_color(Stream::Stdout, |t| t.style(owo_colors::Style::new().yellow().bold())));
}

/// The `## Models` preference lists: conditional `if` rows in file order
/// (first match wins), then one line per criticality that has one.
fn print_model_lists(rules: &PriorityRules) {
    for r in &rules.model_rules {
        println!("  models line {}: if {} → {}", r.line, fmt_conditions(&r.conditions), model_list_colored(&r.models));
    }
    for c in crate::domain::Criticality::ALL {
        let models = rules.model_for(c);
        if !models.is_empty() {
            println!("  models {:<9} {}", format!("{c}:"), model_list_colored(models));
        }
    }
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
    check(ctx, &CheckArgs::default())
}

fn explain(ctx: &mut Context, task_ref: &TaskRef) -> Result<i32> {
    let path = rules_path(ctx)?;
    let full_cfg = ctx.config_or_default()?.clone();
    let cfg = full_cfg.priority.clone();
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
    println!("  model:       {}", with_model_source(model_list_colored(&eval.models), eval.model_source.as_deref()));
    if let Some(note) = eval.models.first().and_then(|m| reservation_note(&full_cfg, m, Some(eval.criticality))) {
        println!("  {} {note}", "note:".if_supports_color(Stream::Stdout, |t| t.dimmed()));
    }
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

/// One row of `priority simulate`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SimRow {
    pub rank: usize,
    pub key: String,
    pub title: String,
    pub state: TaskState,
    /// True when the task is not in the queue yet (`--linear` only).
    pub new: bool,
    pub schedulable: bool,
    /// The task's position with the stored scores (None when it is new or not schedulable).
    pub stored_rank: Option<usize>,
    pub stored_criticality: Criticality,
    pub stored_score: f64,
    pub criticality: Criticality,
    pub score: f64,
    pub skip: bool,
    pub preferred_models: Vec<ModelTier>,
    /// Which `PRIORITY.md` line chose `preferred_models` (see
    /// [`crate::priority::Evaluation::model_source`]).
    pub model_source: Option<String>,
    /// What the budget policy would run (None = throttled or `--no-budget`).
    pub model: Option<ModelTier>,
    /// Why the policy chose (or refused) a model; empty with `--no-budget`.
    pub policy: String,
    /// `true` for the first `scheduler.max_concurrent` schedulable rows.
    pub would_start_now: bool,
    pub reasons: Vec<String>,
}

/// The outcome of one simulation: what [`render_simulation`] prints and
/// what `--json` serialises.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Simulation {
    /// The rules file that was used.
    pub rules: PathBuf,
    pub warnings: Vec<RuleError>,
    pub max_concurrent: u32,
    /// True when a draft rules file or a draft config was used (the daemon
    /// keeps using the live files).
    pub draft: bool,
    pub rows: Vec<SimRow>,
}

/// Dry-run the rules against the queue: every task is re-scored with the
/// given (or live) rules, ranked the way `pick_next` ranks, and run through
/// the budget policy, so the effect of an edit is visible before the daemon
/// picks it up. Nothing is written.
fn simulate(ctx: &mut Context, args: SimulateArgs) -> Result<i32> {
    let path = match &args.file {
        Some(p) => p.clone(),
        None => rules_path(ctx)?,
    };
    let cfg = match &args.config {
        Some(file) => {
            let cfg = Config::load_draft(&ctx.paths, file)?;
            let problems = cfg.validate();
            if !problems.is_empty() {
                bail!("{} is not valid:\n  - {}", file.display(), problems.join("\n  - "));
            }
            cfg
        }
        None => ctx.config_or_default()?.clone(),
    };

    let rules = if path.exists() {
        match parse_file(ctx, &path)? {
            Some(r) => r,
            None => return Ok(1),
        }
    } else if args.file.is_some() {
        bail!("{} does not exist", path.display());
    } else {
        println!(
            "{} {}",
            "note:".if_supports_color(Stream::Stdout, |t| t.style(owo_colors::Style::new().yellow().bold())),
            missing_message(&path)
        );
        PriorityRules::default()
    };
    if !ctx.json {
        print_problems(&rules.warnings, "warning");
    }

    let sim = simulate_with(ctx, &args, &cfg, &rules, &path)?;
    if sim.rows.is_empty() {
        if ctx.json {
            println!("{}", serde_json::json!({ "rules": path, "rows": [] }));
        } else {
            println!(
                "{}",
                "no tasks to rank (add one with `powerqueue add` or sync Linear)"
                    .if_supports_color(Stream::Stdout, |t| t.dimmed())
            );
        }
        return Ok(0);
    }
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&sim)?);
        return Ok(0);
    }
    print!("{}", render_simulation(&sim, &args));
    Ok(0)
}

/// Score and rank the queue with `rules` under `cfg`, exactly as
/// `priority simulate` does, and return the rows (already truncated to
/// `args.limit`). `path` is only recorded for display. Reads tasks, ledgers
/// and cooldowns from the store; with `args.linear` it also fetches the
/// queued Linear issues that have no task yet. Writes nothing.
pub fn simulate_with(
    ctx: &mut Context,
    args: &SimulateArgs,
    cfg: &Config,
    rules: &PriorityRules,
    path: &Path,
) -> Result<Simulation> {
    let store = ctx.store()?.clone();
    let now = Utc::now();
    let mut tasks = if args.all { store.list_tasks()? } else { store.list_open_tasks()? };
    let mut new_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    if args.linear {
        for task in fetch_unqueued_linear_tasks(ctx, cfg, &store)? {
            new_keys.insert(task.key.clone());
            tasks.push(task);
        }
    }
    let draft = args.file.is_some() || args.config.is_some();
    if tasks.is_empty() {
        return Ok(Simulation {
            rules: path.to_path_buf(),
            warnings: rules.warnings.clone(),
            max_concurrent: cfg.scheduler.max_concurrent,
            draft,
            rows: Vec::new(),
        });
    }

    // Rank with the stored scores first, to show what moves.
    let stored_order = ranked(&tasks, now);

    // Re-score every task with the rules under test.
    let jev_enabled = rules.jev.enabled && cfg.priority.jev.enabled;
    let mut evaluated: Vec<Evaluation> = Vec::with_capacity(tasks.len());
    let mut simulated_tasks: Vec<Task> = Vec::with_capacity(tasks.len());
    for task in &tasks {
        let jev = if jev_enabled && !new_keys.contains(&task.key) {
            store.jev_cached(task.id)?.map(|c| {
                let n = rules.jev.levels.len().max(2) as f64;
                (c.score / (n - 1.0)).clamp(0.0, 1.0)
            })
        } else {
            None
        };
        let eval = rules.evaluate(task, now, jev, cfg.priority.jev.weight, cfg.priority.age_boost_per_hour);
        let mut simulated = task.clone();
        simulated.criticality = eval.criticality;
        simulated.score = eval.score;
        if eval.skip && simulated.state == TaskState::Queued {
            simulated.state = TaskState::Paused;
        }
        simulated_tasks.push(simulated);
        evaluated.push(eval);
    }
    let order = ranked(&simulated_tasks, now);

    // Budget policy, with the same reservation the daemon applies between starts.
    let mut policy_input = if args.no_budget { None } else { Some(load_policy_input(cfg, &store, now)?) };
    let mut slots = cfg.scheduler.max_concurrent;
    let mut rows = Vec::with_capacity(order.len());
    for (rank, idx) in order.iter().enumerate() {
        let (task, eval, stored) = (&simulated_tasks[*idx], &evaluated[*idx], &tasks[*idx]);
        let schedulable = task.state.is_schedulable() && task.not_before.is_none_or(|nb| nb <= now);
        // `evaluate` already falls back to the criticality row.
        let preferred = eval.models.clone();
        let model_source = eval.model_source.clone();
        let (model, policy_reason) = match policy_input.as_mut() {
            Some(input) if schedulable => {
                let decision = Policy::new(&cfg.budget, &input.ledgers, &input.limits).decide(
                    task,
                    input.estimator.predict(task),
                    &preferred,
                );
                let reason = match &decision.model {
                    Some(_) => decision.reasons.last().cloned().unwrap_or_default(),
                    None => format!(
                        "throttled until {}",
                        decision.retry_at.map(|r| r.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
                    ),
                };
                if let Some(m) = &decision.model
                    && slots > 0
                {
                    let cost = decision.prediction.weighted_tokens * tier_weight(&cfg.budget, m);
                    reserve(&mut input.ledgers, m, cost);
                }
                (decision.model, reason)
            }
            _ => (None, String::new()),
        };
        let would_start_now = schedulable && (args.no_budget || model.is_some()) && slots > 0;
        if would_start_now {
            slots -= 1;
        }
        let is_new = new_keys.contains(&task.key);
        let stored_rank = stored_order.iter().position(|i| i == idx).map(|r| r + 1);
        rows.push(SimRow {
            rank: rank + 1,
            key: task.key.clone(),
            title: task.title.clone(),
            state: task.state,
            new: is_new,
            schedulable,
            stored_rank: if is_new { None } else { stored_rank },
            stored_criticality: stored.criticality,
            stored_score: stored.score,
            criticality: eval.criticality,
            score: eval.score,
            skip: eval.skip,
            preferred_models: preferred,
            model_source,
            model,
            policy: policy_reason,
            would_start_now,
            reasons: eval.reasons.clone(),
        });
    }
    if args.limit > 0 {
        rows.truncate(args.limit);
    }
    Ok(Simulation {
        rules: path.to_path_buf(),
        warnings: rules.warnings.clone(),
        max_concurrent: cfg.scheduler.max_concurrent,
        draft,
        rows,
    })
}

/// Indices of `tasks` in the order the scheduler would consider them: the
/// schedulable ones first (repeatedly applying [`pick_next`]), then the
/// rest sorted the same way so paused/blocked tasks still show where they
/// would land.
fn ranked(tasks: &[Task], now: DateTime<Utc>) -> Vec<usize> {
    let mut remaining: Vec<usize> = (0..tasks.len()).collect();
    let mut order = Vec::with_capacity(tasks.len());
    loop {
        let candidates: Vec<Task> = remaining.iter().map(|i| tasks[*i].clone()).collect();
        let Some(next) = pick_next(&candidates, now) else { break };
        let pos = remaining.iter().position(|i| tasks[*i].id == next.id).unwrap_or(0);
        order.push(remaining.remove(pos));
    }
    remaining.sort_by(|a, b| {
        let (a, b) = (&tasks[*a], &tasks[*b]);
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.criticality.cmp(&b.criticality))
            .then(a.created_at.cmp(&b.created_at))
    });
    order.extend(remaining);
    order
}

struct PolicyInput {
    ledgers: Ledgers,
    limits: RateLimitState,
    estimator: Estimator,
}

fn load_policy_input(cfg: &Config, store: &Store, now: DateTime<Utc>) -> Result<PolicyInput> {
    let ledgers = Ledgers::load(store, &cfg.budget, now)?;
    let mut limits: RateLimitState = store.kv_get(RATE_LIMITS_KEY)?.unwrap_or_default();
    limits.clear_expired(now);
    let estimator = Estimator::from_summaries(&store.task_usage_summaries()?);
    Ok(PolicyInput { ledgers, limits, estimator })
}

/// Mirror of the daemon's reservation: count a planned start against the
/// ledgers so the next row sees a slightly fuller budget.
fn reserve(ledgers: &mut Ledgers, tier: &ModelTier, weighted_cost: f64) {
    let Some(ledger) = ledgers.for_model_mut(tier) else { return };
    if let Some(t) = ledger.tiers.iter_mut().find(|t| t.tier == *tier) {
        t.period_weighted += weighted_cost;
        t.window_weighted += weighted_cost;
    }
    ledger.total_period_weighted += weighted_cost;
    ledger.total_window_weighted += weighted_cost;
}

/// Queued Linear issues that have no task yet, as unsaved tasks.
fn fetch_unqueued_linear_tasks(ctx: &mut Context, cfg: &Config, store: &Store) -> Result<Vec<Task>> {
    use crate::linear::{IssueFilter, LinearClient, sync::task_from_issue};
    use crate::secrets::SecretKind;
    if !cfg.linear.enabled {
        bail!("linear.enabled is false; `--linear` has nothing to fetch");
    }
    let key = ctx.secrets().require(SecretKind::LinearApiKey)?;
    let client = LinearClient::new(&cfg.linear.endpoint, key)?;
    let rt = super::runtime()?;
    let issues = rt.block_on(client.fetch_issues(&IssueFilter::from_config(&cfg.linear))).context("fetch issues from Linear")?;
    let mut out = Vec::new();
    for issue in &issues {
        if store.get_task_by_linear_issue(&issue.id)?.is_some() || store.get_task_by_key(&issue.identifier)?.is_some() {
            continue;
        }
        out.push(task_from_issue(issue));
    }
    Ok(out)
}

/// The table `priority simulate` prints, plus the `--reasons` detail and the
/// summary line, as text (colours follow the stdout colour setting).
pub fn render_simulation(sim: &Simulation, args: &SimulateArgs) -> String {
    use std::fmt::Write as _;
    let (path, rows) = (&sim.rules, &sim.rows);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {} {}",
        "Simulated queue with".if_supports_color(Stream::Stdout, |t| t.bold()),
        path.display(),
        format!("(max {} concurrent; nothing was written)", sim.max_concurrent).if_supports_color(Stream::Stdout, |t| t.dimmed())
    );
    let mut t = table();
    let mut header = vec!["#", "was", "task", "state", "criticality", "score", "prefers"];
    if !args.no_budget {
        header.push("policy would run");
    }
    header.push("title");
    t.set_header(header);
    for r in rows {
        let rank = if r.would_start_now { format!("{} ▶", r.rank) } else { r.rank.to_string() };
        let was = match (r.new, r.stored_rank) {
            (true, _) => "new".to_string(),
            (false, Some(s)) if s == r.rank => "=".to_string(),
            (false, Some(s)) if s > r.rank => format!("↑{s}"),
            (false, Some(s)) => format!("↓{s}"),
            (false, None) => "–".to_string(),
        };
        let crit = if r.stored_criticality != r.criticality && !r.new {
            format!("{} → {}", r.stored_criticality, criticality_colored(r.criticality))
        } else {
            criticality_colored(r.criticality)
        };
        let score = if !r.new && (r.stored_score - r.score).abs() > 0.5 {
            format!("{:.0} → {:.0}", r.stored_score, r.score)
        } else {
            format!("{:.0}", r.score)
        };
        let state = if r.skip {
            "skip".if_supports_color(Stream::Stdout, |t| t.red()).to_string()
        } else {
            crate::cli::output::state_colored(r.state)
        };
        // Only conditional rows are worth the width; the criticality row is implied.
        let preferred = match r.model_source.as_deref() {
            Some(src) if src.starts_with("if ") => with_model_source(model_list_colored(&r.preferred_models), Some(src)),
            _ => model_list_colored(&r.preferred_models),
        };
        let mut row = vec![rank, was, r.key.clone(), state, crit, score, preferred];
        if !args.no_budget {
            let policy = match (&r.model, r.schedulable) {
                (Some(m), _) => model_colored(Some(m)),
                (None, true) if !r.policy.is_empty() => r.policy.clone(),
                (None, _) => "–".to_string(),
            };
            row.push(policy);
        }
        row.push(truncate(&r.title, 40));
        t.add_row(row);
    }
    let _ = writeln!(out, "{t}");
    if args.reasons {
        for r in rows {
            let _ = writeln!(out, "\n{} {}", r.key.if_supports_color(Stream::Stdout, |t| t.bold()), truncate(&r.title, 60));
            for reason in &r.reasons {
                let _ = writeln!(out, "    · {reason}");
            }
            if !r.policy.is_empty() {
                let _ = writeln!(out, "    · policy: {}", r.policy);
            }
        }
    }
    let moved = rows.iter().filter(|r| !r.new && r.stored_rank.is_some_and(|s| s != r.rank)).count();
    let recrit = rows.iter().filter(|r| !r.new && r.stored_criticality != r.criticality).count();
    let skipped = rows.iter().filter(|r| r.skip).count();
    let starting = rows.iter().filter(|r| r.would_start_now).count();
    let _ = writeln!(
        out,
        "{}",
        format!(
            "{} task(s); {starting} would start now (▶); {moved} change rank; {recrit} change criticality; {skipped} skipped. \
             `was` = rank with the stored scores; ↑/↓ = moved up/down from that rank.{}",
            rows.len(),
            if sim.draft {
                " The daemon still uses the live files."
            } else {
                " The daemon applies the live file on its next tick."
            }
        )
        .if_supports_color(Stream::Stdout, |t| t.dimmed())
    );
    out
}
