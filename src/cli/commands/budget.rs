//! `powerqueue budget ...`.
//!
//! `show` prints one section per enabled provider (in `provider_order`):
//! period, spend, the learned exchange rate, observed usage with its age,
//! anchor source, cooldowns and the model table, then what the policy would
//! run now: one row per criticality for a typical task, and one row per
//! task actually waiting in the queue.
//! `probe` asks providers for their remaining allowance. `set-reset`,
//! `set-observed` and `clear-limits` take `--provider <p>` (default `claude`).

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use owo_colors::{OwoColorize, Stream};

use crate::budget::probes::describe;
use crate::budget::{
    Estimator, Ledger, Ledgers, ObservedUsage, PeriodClock, Policy, RATE_LIMITS_KEY, RateLimitState, TierLedger,
    load_observations, load_observed, load_probe_status, probe_providers, save_observed, tier_weight,
};
use crate::cli::output::{human_duration, human_f64, model_colored, table};
use crate::cli::{BudgetCommand, Context};
use crate::config::Config;
use crate::domain::{Criticality, ModelTier, Provider, Task, TaskSource};
use crate::priority::PriorityRules;
use crate::scheduler::{Candidate, LaunchContext, LaunchPlanner};

pub fn run(ctx: &mut Context, cmd: BudgetCommand) -> Result<i32> {
    match cmd {
        BudgetCommand::Show => show(ctx),
        BudgetCommand::SetReset { when, provider } => set_reset(ctx, &when, provider),
        BudgetCommand::SetObserved { percent, provider } => set_observed(ctx, &percent, provider),
        BudgetCommand::Probe { provider } => probe(ctx, provider),
        BudgetCommand::ClearLimits { provider } => clear_limits(ctx, provider),
        BudgetCommand::Estimate { task } => estimate(ctx, task.as_deref()),
    }
}

/// `budget probe`: run the probes now (one provider, or every enabled one),
/// store what they learn and print it. Exit code 1 when any probe failed.
fn probe(ctx: &mut Context, provider: Option<Provider>) -> Result<i32> {
    let cfg = ctx.config_cloned()?;
    let store = ctx.store()?.clone();
    let providers = match provider {
        Some(p) => vec![p],
        None => cfg.budget.enabled_providers_in_order(),
    };
    if providers.is_empty() {
        bail!("no provider is enabled; set budget.providers.claude.enabled = true");
    }
    let now = Utc::now();
    let results = probe_providers(&cfg, &store, &providers, now);
    let failed = results.iter().any(|(_, r)| r.is_err());
    if ctx.json {
        let mut out = serde_json::Map::new();
        for (p, r) in &results {
            let v = match r {
                Ok(obs) => serde_json::json!({ "ok": true, "observed": obs, "error": null }),
                Err(e) => serde_json::json!({ "ok": false, "observed": null, "error": format!("{e:#}") }),
            };
            out.insert(p.to_string(), v);
        }
        println!("{}", serde_json::to_string_pretty(&serde_json::Value::Object(out))?);
        return Ok(if failed { 1 } else { 0 });
    }
    let pct = |v: Option<f64>| v.map(|f| format!("{:.0}%", f * 100.0)).unwrap_or_else(|| "-".to_string());
    let when =
        |v: Option<DateTime<Utc>>| v.map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_else(|| "-".to_string());
    let mut t = table();
    t.set_header(["provider", "result", "window", "window resets", "period", "period resets", "note"]);
    for (p, r) in &results {
        let row = match r {
            Ok(Some(obs)) => [
                p.to_string(),
                "ok".if_supports_color(Stream::Stdout, |t| t.green()).to_string(),
                pct(obs.window_used),
                when(obs.window_resets_at),
                pct(obs.period_used),
                when(obs.period_resets_at),
                if obs.blocked { "blocked".to_string() } else { String::new() },
            ],
            Ok(None) => {
                let note = match p {
                    Provider::Claude => "nothing yet: a session's status line reports it after the first response",
                    _ => "the provider reported nothing usable",
                };
                [p.to_string(), "unknown".into(), "-".into(), "-".into(), "-".into(), "-".into(), note.to_string()]
            }
            Err(e) => [
                p.to_string(),
                "failed".if_supports_color(Stream::Stdout, |t| t.red()).to_string(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                format!("{e:#}"),
            ],
        };
        t.add_row(row);
    }
    println!("{t}");
    Ok(if failed { 1 } else { 0 })
}

fn load_ledgers(ctx: &mut Context, now: DateTime<Utc>) -> Result<(Config, Ledgers, RateLimitState)> {
    let cfg = ctx.config_cloned()?;
    let store = ctx.store()?;
    let ledgers = Ledgers::load(store, &cfg.budget, now)?;
    let mut limits: RateLimitState = store.kv_get(RATE_LIMITS_KEY)?.unwrap_or_default();
    limits.clear_expired(now);
    Ok((cfg, ledgers, limits))
}

/// Human status of one tier given the period pacing.
fn tier_status(t: &TierLedger, elapsed: f64, limits: &RateLimitState, now: DateTime<Utc>) -> &'static str {
    if let Some(_until) = limits.until(&t.tier, now) {
        return "rate-limited";
    }
    if t.period_budget <= 0.0 {
        return "disabled";
    }
    let spent = t.period_spent_fraction();
    if spent >= 1.0 {
        "exhausted"
    } else if spent > elapsed + 0.10 {
        "over-paced"
    } else if spent + 0.10 < elapsed {
        "under-paced"
    } else {
        "ok"
    }
}

/// A normal-criticality, default-cost task: what `show` asks the policy about.
fn probe_task(crit: Criticality) -> Task {
    let mut task = Task::new("probe", "probe", TaskSource::Manual);
    task.criticality = crit;
    task
}

/// Until when a provider is cooling down: every enabled model rate-limited,
/// or its observation says the allowance is exhausted (the later wins).
fn provider_cooldown(cfg: &Config, ledger: &Ledger, limits: &RateLimitState, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let marks = limits.provider_blocked_until(&cfg.budget, ledger.provider, now);
    let observed = ledger.observed.as_ref().and_then(|o| o.cooldown_until()).filter(|u| *u > now);
    match (marks, observed) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Machine-readable view of one provider for `budget show --json`: the
/// ledger plus the observation's age, the latest probe outcome and any
/// cooldown.
fn provider_json(
    cfg: &Config,
    store: &crate::store::Store,
    ledger: &Ledger,
    limits: &RateLimitState,
    now: DateTime<Utc>,
) -> Result<serde_json::Value> {
    let mut v = serde_json::to_value(ledger)?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "observed_age_secs".into(),
            serde_json::json!(ledger.observed.as_ref().map(|o| (now - o.observed_at).num_seconds().max(0))),
        );
        obj.insert("probe".into(), serde_json::to_value(load_probe_status(store, ledger.provider)?)?);
        obj.insert("cooldown_until".into(), serde_json::json!(provider_cooldown(cfg, ledger, limits, now)));
        let limited: serde_json::Map<String, serde_json::Value> = limits
            .exhausted_until
            .iter()
            .filter(|(t, u)| t.provider() == ledger.provider && **u > now)
            .map(|(t, u)| (t.to_string(), serde_json::json!(u)))
            .collect();
        obj.insert("rate_limited_until".into(), serde_json::Value::Object(limited));
    }
    Ok(v)
}

fn show(ctx: &mut Context) -> Result<i32> {
    let now = Utc::now();
    let (cfg, ledgers, limits) = load_ledgers(ctx, now)?;
    if ledgers.is_empty() {
        bail!("no provider is enabled; set budget.providers.claude.enabled = true");
    }
    let store = ctx.store()?.clone();
    let ordered = ledgers.ordered(&cfg.budget.provider_order);
    let policy = Policy::new(&cfg.budget, &ledgers, &limits);
    let estimator = Estimator::from_summaries(&store.task_usage_summaries()?);
    if ctx.json {
        let mut providers = serde_json::Map::new();
        for ledger in &ordered {
            providers.insert(ledger.provider.to_string(), provider_json(&cfg, &store, ledger, &limits, now)?);
        }
        let next = policy.decide(&probe_task(Criticality::Normal), estimator.predict(&probe_task(Criticality::Normal)), &[]);
        let queued: Vec<serde_json::Value> = queued_decisions(&cfg, &ctx.paths, &store, &ledgers, &limits, &estimator, now)?
            .into_iter()
            .map(|q| serde_json::json!({ "key": q.task.key, "criticality": q.task.criticality, "state": q.task.state, "preferred": q.preferred, "decision": q.decision }))
            .collect();
        let out = serde_json::json!({
            "providers": providers,
            "provider_order": cfg.budget.enabled_providers_in_order(),
            "next": next,
            "queued": queued,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    for (i, ledger) in ordered.iter().enumerate() {
        if i > 0 {
            println!();
        }
        show_provider(&cfg, &store, ledger, &limits, now)?;
    }
    println!(
        "{}",
        "pace = spent% − elapsed%; negative means the model is under-spent and will relax to lower criticalities"
            .if_supports_color(Stream::Stdout, |t| t.dimmed())
    );
    let disabled: Vec<String> = Provider::ALL.iter().filter(|p| ledgers.get(**p).is_none()).map(|p| p.to_string()).collect();
    if !disabled.is_empty() {
        println!(
            "\n{}",
            format!("disabled providers: {} (budget.providers.<name>.enabled)", disabled.join(", "))
                .if_supports_color(Stream::Stdout, |t| t.dimmed())
        );
    }

    let typical = estimator.predict(&probe_task(Criticality::Normal));
    println!(
        "\n{} (a typical task: {} weighted tokens, {}; no overrides)",
        "what would run now".if_supports_color(Stream::Stdout, |t| t.bold()),
        human_f64(typical.weighted_tokens),
        typical.basis
    );
    let mut t = table();
    t.set_header(["criticality", "model", "why"]);
    for crit in Criticality::ALL {
        let task = probe_task(crit);
        let d = policy.decide(&task, estimator.predict(&task), &[]);
        t.add_row([crit.to_string(), model_colored(d.model.as_ref()), decision_why(&d)]);
    }
    println!("{t}");

    let queued = queued_decisions(&cfg, &ctx.paths, &store, &ledgers, &limits, &estimator, now)?;
    if !queued.is_empty() {
        println!(
            "\n{} (in the daemon's order, each start reserving its predicted cost; tasks it would skip last)",
            "queued tasks".if_supports_color(Stream::Stdout, |t| t.bold())
        );
        let mut t = table();
        t.set_header(["task", "crit", "state", "predicted", "wants", "model", "why"]);
        for q in &queued {
            let wants = if q.preferred.is_empty() {
                "-".to_string()
            } else {
                q.preferred.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(" | ")
            };
            t.add_row([
                q.task.key.clone(),
                q.task.criticality.to_string(),
                q.task.state.to_string(),
                human_f64(q.decision.prediction.weighted_tokens),
                wants,
                model_colored(q.decision.model.as_ref()),
                decision_why(&q.decision),
            ]);
        }
        println!("{t}");
    }
    Ok(0)
}

/// The last reason of a decision, or when a throttled task is looked at again.
fn decision_why(d: &crate::budget::Decision) -> String {
    match d.model {
        Some(_) => d.reasons.last().cloned().unwrap_or_default(),
        None => format!("throttled until {}", d.retry_at.map(|r| r.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()),
    }
}

/// The policy's answer for every schedulable open task (queued, throttled,
/// crashed), in the order the daemon's next pass would take them
/// (`pick_next`: score, criticality, age; a started task's predicted cost
/// is reserved before the next one is judged), followed by the tasks that
/// pass skips (waiting on a dependency, or a retry time still ahead). Each
/// task gets its own prediction and the rules' preferred models (`task
/// model` overrides are read by the policy itself). Fails when PRIORITY.md
/// cannot be parsed or the store cannot be read.
fn queued_decisions(
    cfg: &Config,
    paths: &crate::paths::Paths,
    store: &crate::store::Store,
    ledgers: &Ledgers,
    limits: &RateLimitState,
    estimator: &Estimator,
    now: DateTime<Utc>,
) -> Result<Vec<Candidate>> {
    let rules = PriorityRules::from_source(&cfg.rules_source(paths))?.unwrap_or_default();
    let tasks: Vec<Task> = store.list_open_tasks()?.into_iter().filter(|t| t.state.is_schedulable()).collect();
    // The daemon's planner with no slot limit: what the queue would get if
    // every task could start.
    let planner = LaunchPlanner::new(estimator.clone(), ledgers.clone(), tasks, u32::MAX);
    Ok(planner.plan_all(LaunchContext { budget: &cfg.budget, rules: &rules, rate_limits: limits }, now))
}

/// Print one provider's section of `budget show`.
fn show_provider(
    cfg: &Config,
    store: &crate::store::Store,
    ledger: &Ledger,
    limits: &RateLimitState,
    now: DateTime<Utc>,
) -> Result<()> {
    let provider = ledger.provider;
    let elapsed = ledger.elapsed_fraction();
    let anchor_note = match ledger.anchor_source {
        crate::budget::AnchorSource::Default => {
            format!("  [anchor not set: run `powerqueue budget set-reset --provider {provider}` or `budget probe`]")
                .if_supports_color(Stream::Stdout, |t| t.yellow())
                .to_string()
        }
        crate::budget::AnchorSource::Observed => "  [anchor learned from the provider]".to_string(),
        crate::budget::AnchorSource::Config => "  [anchor from config]".to_string(),
    };
    println!(
        "{} {} {} → {}  ({:.0}% elapsed, {} left){}",
        provider.if_supports_color(Stream::Stdout, |t| t.bold()),
        "period".if_supports_color(Stream::Stdout, |t| t.dimmed()),
        ledger.period.start.format("%Y-%m-%d %H:%M UTC"),
        ledger.period.end.format("%Y-%m-%d %H:%M UTC"),
        elapsed * 100.0,
        human_duration(ledger.period.remaining(now).num_seconds()),
        anchor_note
    );
    let window_text = if ledger.has_window() {
        format!(
            "; window {:.0}% ({}, {} of {} weighted tokens measured in it)",
            ledger.window_fraction() * 100.0,
            ledger.window_fraction_source(),
            human_f64(ledger.total_window_weighted),
            human_f64(ledger.window_budget),
        )
    } else {
        "; no rolling window".to_string()
    };
    println!(
        "{} {:.0}% of the period allowance ({}){}",
        "used  ".if_supports_color(Stream::Stdout, |t| t.bold()),
        ledger.period_fraction() * 100.0,
        ledger.period_fraction_source(),
        window_text
    );
    println!(
        "{} {} of {} weighted tokens ({:.0}% of the {} budget)",
        "spent ".if_supports_color(Stream::Stdout, |t| t.bold()),
        human_f64(ledger.total_period_weighted),
        human_f64(ledger.period_budget),
        ledger.measured_period_fraction() * 100.0,
        if ledger.learned.period.is_some() { "learned" } else { "configured" },
    );
    let readings = load_observations(store, provider)?.iter().filter(|r| ledger.period.contains(r.at)).count();
    match ledger.learned.period {
        Some(r) => println!(
            "{} 100% of the period ≈ {} weighted tokens (configured {}): {:.1} points moved for {} tokens between {} and {}, {} readings",
            "rate  ".if_supports_color(Stream::Stdout, |t| t.bold()),
            human_f64(r.budget),
            human_f64(ledger.configured_period_budget),
            r.observed_delta * 100.0,
            human_f64(r.measured_delta),
            r.from.format("%m-%d %H:%M"),
            r.to.format("%m-%d %H:%M UTC"),
            r.samples
        ),
        None => println!(
            "{} not learned yet ({readings} reading(s) this period; needs two at least {:.0} points and {} tokens apart); using the configured {} weighted tokens",
            "rate  ".if_supports_color(Stream::Stdout, |t| t.bold()),
            crate::budget::MIN_OBSERVED_DELTA * 100.0,
            human_f64(crate::budget::MIN_MEASURED_DELTA),
            human_f64(ledger.configured_period_budget)
        ),
    }
    if let Some(w) = ledger.learned.window {
        println!(
            "       100% of the window ≈ {} weighted tokens (configured {}): {:.1} points for {} tokens over {}",
            human_f64(w.budget),
            human_f64(ledger.configured_window_budget),
            w.observed_delta * 100.0,
            human_f64(w.measured_delta),
            human_duration((w.to - w.from).num_seconds())
        );
    }
    match &ledger.observed {
        Some(obs) => {
            let age = human_duration((now - obs.observed_at).num_seconds().max(0));
            println!("{} {} ({age} ago)", "observed".if_supports_color(Stream::Stdout, |t| t.bold()), describe(obs));
        }
        None => {
            let how = match provider {
                Provider::Claude => "a session's status line reports it after the first response",
                _ => "run `powerqueue budget probe`",
            };
            println!("{} none yet; {how}", "observed".if_supports_color(Stream::Stdout, |t| t.bold()));
        }
    }
    if let Some(status) = load_probe_status(store, provider)?
        && let Some(err) = &status.error
    {
        println!(
            "{}",
            format!("probe    last run {} failed: {err}", status.at.format("%Y-%m-%d %H:%M UTC"))
                .if_supports_color(Stream::Stdout, |t| t.yellow())
        );
    }
    if let Some(until) = provider_cooldown(cfg, ledger, limits, now) {
        println!(
            "{}",
            format!("cooldown {provider} is not scheduled until {}", until.format("%Y-%m-%d %H:%M UTC"))
                .if_supports_color(Stream::Stdout, |t| t.yellow())
        );
    }

    let mut t = table();
    t.set_header(["model", "budget", "spent", "%", "pace", "window", "msgs", "status"]);
    for tier in &ledger.tiers {
        let status = tier_status(tier, elapsed, limits, now);
        let status = match status {
            "ok" => status.if_supports_color(Stream::Stdout, |t| t.green()).to_string(),
            "under-paced" => status.if_supports_color(Stream::Stdout, |t| t.cyan()).to_string(),
            "over-paced" | "rate-limited" => status.if_supports_color(Stream::Stdout, |t| t.yellow()).to_string(),
            "exhausted" => status.if_supports_color(Stream::Stdout, |t| t.red()).to_string(),
            other => other.if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string(),
        };
        let until = limits.until(&tier.tier, now).map(|u| format!(" until {}", u.format("%H:%M UTC"))).unwrap_or_default();
        t.add_row([
            model_colored(Some(&tier.tier)),
            human_f64(tier.period_budget),
            human_f64(tier.period_weighted),
            format!("{:.0}%", tier.period_spent_fraction() * 100.0),
            format!("{:+.0}", (tier.period_spent_fraction() - elapsed) * 100.0),
            human_f64(tier.window_weighted),
            tier.messages.to_string(),
            format!("{status}{until}"),
        ]);
    }
    println!("{t}");
    Ok(())
}

/// Parse `<when>`: RFC 3339, `in 3d4h`, or a bare duration like `90m`.
pub fn parse_reset(when: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let s = when.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    let rel = s.strip_prefix("in ").map(str::trim).unwrap_or(s);
    let d = humantime::parse_duration(rel)
        .map_err(|e| anyhow!("`{when}` is neither RFC 3339 (2026-10-06T07:00:00Z) nor a duration like `in 3d4h`: {e}"))?;
    let d = Duration::from_std(d).context("duration too large")?;
    Ok(now + d)
}

fn set_reset(ctx: &mut Context, when: &str, provider: Provider) -> Result<i32> {
    let now = Utc::now();
    let anchor = parse_reset(when, now)?;
    // Reload from the file so repo overrides are not written back into config.toml.
    let mut cfg = Config::load(&ctx.paths)?;
    cfg.budget.providers.get_mut(provider).period_anchor = Some(anchor.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    cfg.save(&ctx.paths)?;
    let clock = PeriodClock::from_provider(cfg.budget.provider(provider), now);
    let period = clock.current_period(now);
    if ctx.json {
        println!(
            "{}",
            serde_json::json!({ "provider": provider, "anchor": anchor, "period_start": period.start, "period_end": period.end })
        );
    } else {
        println!(
            "{provider} period anchor set to {}; current period {} → {} ({} until reset)",
            anchor.format("%Y-%m-%d %H:%M UTC"),
            period.start.format("%Y-%m-%d %H:%M UTC"),
            period.end.format("%Y-%m-%d %H:%M UTC"),
            human_duration(period.remaining(now).num_seconds())
        );
    }
    Ok(0)
}

/// Parse `43%`, `43` or `43.5` into a fraction.
pub fn parse_percent(s: &str) -> Result<f64> {
    let v: f64 = s.trim().trim_end_matches('%').trim().parse().map_err(|_| anyhow!("`{s}` is not a percentage like `43%`"))?;
    if !(0.0..=200.0).contains(&v) {
        bail!("`{s}` is out of range (0–200%)");
    }
    Ok(v / 100.0)
}

fn set_observed(ctx: &mut Context, percent: &str, provider: Provider) -> Result<i32> {
    let observed = parse_percent(percent)?;
    let now = Utc::now();
    let cfg = ctx.config_cloned()?;
    if !cfg.budget.provider(provider).enabled {
        bail!("provider {provider} is not enabled (budget.providers.{provider}.enabled = false)");
    }
    let store = ctx.store()?.clone();
    // A manual reading keeps what the last probe knew (resets, the window,
    // a blocked flag); only the period percentage and the instant change.
    let previous = load_observed(&store, provider)?;
    let obs =
        ObservedUsage { period_used: Some(observed), observed_at: now, ..previous.unwrap_or_else(|| ObservedUsage::empty(now)) };
    save_observed(&store, provider, &obs)?;
    let ledger = Ledger::load(&store, &cfg.budget, provider, now)?;
    if ctx.json {
        println!("{}", serde_json::json!({ "provider": provider, "observed": obs, "learned": ledger.learned }));
        return Ok(0);
    }
    match ledger.learned.period {
        Some(r) => println!(
            "recorded {provider} at {:.0}%; rate learned: 100% ≈ {} weighted tokens (configured {})",
            observed * 100.0,
            human_f64(r.budget),
            human_f64(ledger.configured_period_budget)
        ),
        None => println!(
            "recorded {provider} at {:.0}%; pacing starts from that reading (a second one at least {:.0} points later teaches the rate)",
            observed * 100.0,
            crate::budget::MIN_OBSERVED_DELTA * 100.0
        ),
    }
    Ok(0)
}

fn clear_limits(ctx: &mut Context, provider: Provider) -> Result<i32> {
    let store = ctx.store()?;
    let mut limits: RateLimitState = store.kv_get(RATE_LIMITS_KEY)?.unwrap_or_default();
    limits.clear_provider(provider);
    if limits.is_empty() {
        store.kv_delete(RATE_LIMITS_KEY)?;
    } else {
        store.kv_set(RATE_LIMITS_KEY, &limits)?;
    }
    if ctx.json {
        println!("{}", serde_json::json!({ "cleared": true, "provider": provider }));
    } else {
        println!("{provider} rate-limit cooldowns cleared");
    }
    Ok(0)
}

fn estimate(ctx: &mut Context, task: Option<&str>) -> Result<i32> {
    let cfg = ctx.config_cloned()?;
    let store = ctx.store()?.clone();
    let estimator = Estimator::from_summaries(&store.task_usage_summaries()?);
    match task {
        Some(needle) => {
            let task = store.find_task(needle)?.ok_or_else(|| anyhow!("no task matches `{needle}`"))?;
            let p = estimator.predict(&task);
            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "task": task.key, "prediction": p }))?);
            } else {
                println!("{} {} ({})", "task".if_supports_color(Stream::Stdout, |t| t.bold()), task.key, task.title);
                println!(
                    "  weighted tokens: {} (≈ {} on sonnet, {} on fable)",
                    human_f64(p.weighted_tokens),
                    human_f64(p.weighted_tokens * tier_weight(&cfg.budget, &ModelTier::sonnet())),
                    human_f64(p.weighted_tokens * tier_weight(&cfg.budget, &ModelTier::fable()))
                );
                println!("  wall clock:      {}", human_duration(p.wall_secs as i64));
                println!("  confidence:      {:.0}% — {}", p.confidence * 100.0, p.basis);
            }
        }
        None => {
            let accuracy = estimator.accuracy();
            if ctx.json {
                println!("{}", serde_json::json!({ "samples": estimator.sample_count(), "mape": accuracy }));
            } else {
                println!(
                    "{} {} task(s) with usage history",
                    "samples".if_supports_color(Stream::Stdout, |t| t.bold()),
                    estimator.sample_count()
                );
                match accuracy {
                    Some(m) => {
                        println!(
                            "{} leave-one-out predictions are off by {:.0}% on average",
                            "accuracy".if_supports_color(Stream::Stdout, |t| t.bold()),
                            m * 100.0
                        )
                    }
                    None => println!(
                        "{} not enough completed tasks yet (need 4)",
                        "accuracy".if_supports_color(Stream::Stdout, |t| t.bold())
                    ),
                }
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn parses_reset_instants() {
        let now = at("2026-10-01T12:00:00Z");
        assert_eq!(parse_reset("2026-10-06T09:00:00+02:00", now).unwrap(), at("2026-10-06T07:00:00Z"));
        assert_eq!(parse_reset("in 3d4h", now).unwrap(), at("2026-10-04T16:00:00Z"));
        assert_eq!(parse_reset("90m", now).unwrap(), at("2026-10-01T13:30:00Z"));
        assert!(parse_reset("tomorrow", now).is_err());
    }

    #[test]
    fn parses_percentages() {
        assert_eq!(parse_percent("43%").unwrap(), 0.43);
        assert_eq!(parse_percent(" 12.5 ").unwrap(), 0.125);
        assert!(parse_percent("abc").is_err());
        assert!(parse_percent("250%").is_err());
    }

    #[test]
    fn tier_status_labels() {
        let now = Utc::now();
        let limits = RateLimitState::default();
        let t = TierLedger { tier: ModelTier::opus(), period_budget: 100.0, period_weighted: 50.0, ..Default::default() };
        assert_eq!(tier_status(&t, 0.5, &limits, now), "ok");
        assert_eq!(tier_status(&t, 0.9, &limits, now), "under-paced");
        assert_eq!(tier_status(&t, 0.1, &limits, now), "over-paced");
        let t = TierLedger { period_weighted: 100.0, ..t };
        assert_eq!(tier_status(&t, 0.5, &limits, now), "exhausted");
        let t = TierLedger { period_budget: 0.0, ..t };
        assert_eq!(tier_status(&t, 0.5, &limits, now), "disabled");
        let mut limits = RateLimitState::default();
        limits.mark(ModelTier::opus(), now + Duration::minutes(5));
        assert_eq!(tier_status(&t, 0.5, &limits, now), "rate-limited");
    }
}
