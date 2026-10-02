//! `powerqueue budget ...`.

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use owo_colors::{OwoColorize, Stream};

use crate::budget::{
    CALIBRATION_KEY, Calibration, Estimator, Ledger, PeriodClock, Policy, RATE_LIMITS_KEY, RateLimitState, TierLedger,
};
use crate::cli::output::{human_duration, human_f64, model_colored, table};
use crate::cli::{BudgetCommand, Context};
use crate::config::Config;
use crate::domain::{Criticality, ModelTier, Task, TaskSource};

pub fn run(ctx: &mut Context, cmd: BudgetCommand) -> Result<i32> {
    match cmd {
        BudgetCommand::Show => show(ctx),
        BudgetCommand::SetReset { when } => set_reset(ctx, &when),
        BudgetCommand::SetObserved { percent } => set_observed(ctx, &percent),
        BudgetCommand::ClearLimits => clear_limits(ctx),
        BudgetCommand::Estimate { task } => estimate(ctx, task.as_deref()),
    }
}

fn load_ledger(ctx: &mut Context, now: DateTime<Utc>) -> Result<(Config, Ledger, RateLimitState)> {
    let cfg = ctx.config_cloned()?;
    let store = ctx.store()?;
    let clock = PeriodClock::from_config(&cfg.budget, now);
    let ledger = Ledger::load(store, &cfg.budget, &clock, now)?;
    let mut limits: RateLimitState = store.kv_get(RATE_LIMITS_KEY)?.unwrap_or_default();
    limits.clear_expired(now);
    Ok((cfg, ledger, limits))
}

/// Human status of one tier given the period pacing.
fn tier_status(t: &TierLedger, elapsed: f64, limits: &RateLimitState, now: DateTime<Utc>) -> &'static str {
    if let Some(_until) = limits.until(t.tier, now) {
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

fn show(ctx: &mut Context) -> Result<i32> {
    let now = Utc::now();
    let (cfg, ledger, limits) = load_ledger(ctx, now)?;
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&ledger)?);
        return Ok(0);
    }
    let elapsed = ledger.elapsed_fraction();
    println!(
        "{} {} → {}  ({:.0}% elapsed, {} left){}",
        "period".if_supports_color(Stream::Stdout, |t| t.bold()),
        ledger.period.start.format("%Y-%m-%d %H:%M UTC"),
        ledger.period.end.format("%Y-%m-%d %H:%M UTC"),
        elapsed * 100.0,
        human_duration(ledger.period.remaining(now).num_seconds()),
        if cfg.budget.period_anchor.is_none() {
            "  [anchor not set: run `powerqueue budget set-reset`]".if_supports_color(Stream::Stdout, |t| t.yellow()).to_string()
        } else {
            String::new()
        }
    );
    println!(
        "{} {} of {} weighted tokens ({:.0}% measured{}); window {} of {} ({:.0}%)",
        "spent ".if_supports_color(Stream::Stdout, |t| t.bold()),
        human_f64(ledger.total_period_weighted),
        human_f64(ledger.period_budget),
        ledger.measured_period_fraction() * 100.0,
        match ledger.calibration {
            Some(_) => format!(", {:.0}% calibrated", ledger.period_fraction() * 100.0),
            None => String::new(),
        },
        human_f64(ledger.total_window_weighted),
        human_f64(ledger.window_budget),
        ledger.window_fraction() * 100.0
    );
    match ledger.calibration {
        Some(c) => println!(
            "{} observed {:.0}% at {} when we measured {:.0}% (offset {:+.0} points)",
            "calibration".if_supports_color(Stream::Stdout, |t| t.bold()),
            c.observed_fraction * 100.0,
            c.at.format("%Y-%m-%d %H:%M UTC"),
            c.measured_fraction * 100.0,
            c.offset() * 100.0
        ),
        None => println!(
            "{} none this period (use `powerqueue budget set-observed <percent>` after checking /usage)",
            "calibration".if_supports_color(Stream::Stdout, |t| t.bold())
        ),
    }

    let mut t = table();
    t.set_header(["tier", "budget", "spent", "%", "pace", "window", "msgs", "status"]);
    for tier in &ledger.tiers {
        let status = tier_status(tier, elapsed, &limits, now);
        let status = match status {
            "ok" => status.if_supports_color(Stream::Stdout, |t| t.green()).to_string(),
            "under-paced" => status.if_supports_color(Stream::Stdout, |t| t.cyan()).to_string(),
            "over-paced" | "rate-limited" => status.if_supports_color(Stream::Stdout, |t| t.yellow()).to_string(),
            "exhausted" => status.if_supports_color(Stream::Stdout, |t| t.red()).to_string(),
            other => other.if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string(),
        };
        let until = limits.until(tier.tier, now).map(|u| format!(" until {}", u.format("%H:%M UTC"))).unwrap_or_default();
        t.add_row([
            model_colored(Some(tier.tier)),
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
    println!(
        "{}",
        "pace = spent% − elapsed%; negative means the tier is under-spent and will relax to lower criticalities"
            .if_supports_color(Stream::Stdout, |t| t.dimmed())
    );

    println!("\n{} (default-cost task, no overrides)", "what would run now".if_supports_color(Stream::Stdout, |t| t.bold()));
    let policy = Policy::new(&cfg.budget, &ledger, &limits);
    let mut t = table();
    t.set_header(["criticality", "model", "why"]);
    for crit in Criticality::ALL {
        let mut task = Task::new("probe", "probe", TaskSource::Manual);
        task.criticality = crit;
        let d = policy.decide(&task, crate::budget::Prediction::default_guess(), None);
        let why = match d.model {
            Some(_) => d.reasons.last().cloned().unwrap_or_default(),
            None => {
                format!("throttled until {}", d.retry_at.map(|r| r.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default())
            }
        };
        t.add_row([crit.to_string(), model_colored(d.model), why]);
    }
    println!("{t}");
    Ok(0)
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

fn set_reset(ctx: &mut Context, when: &str) -> Result<i32> {
    let now = Utc::now();
    let anchor = parse_reset(when, now)?;
    // Reload from the file so repo overrides are not written back into config.toml.
    let mut cfg = Config::load(&ctx.paths)?;
    cfg.budget.period_anchor = Some(anchor.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    cfg.save(&ctx.paths)?;
    let clock = PeriodClock::from_config(&cfg.budget, now);
    let period = clock.current_period(now);
    if ctx.json {
        println!("{}", serde_json::json!({ "anchor": anchor, "period_start": period.start, "period_end": period.end }));
    } else {
        println!(
            "period anchor set to {}; current period {} → {} ({} until reset)",
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

fn set_observed(ctx: &mut Context, percent: &str) -> Result<i32> {
    let observed = parse_percent(percent)?;
    let now = Utc::now();
    let (_, ledger, _) = load_ledger(ctx, now)?;
    let cal = Calibration { observed_fraction: observed, at: now, measured_fraction: ledger.measured_period_fraction() };
    ctx.store()?.kv_set(CALIBRATION_KEY, &cal)?;
    if ctx.json {
        println!("{}", serde_json::to_string(&cal)?);
    } else {
        println!(
            "calibrated: /usage shows {:.0}%, powerqueue measured {:.0}% → offset {:+.0} points applied for the rest of this period",
            observed * 100.0,
            cal.measured_fraction * 100.0,
            cal.offset() * 100.0
        );
    }
    Ok(0)
}

fn clear_limits(ctx: &mut Context) -> Result<i32> {
    ctx.store()?.kv_delete(RATE_LIMITS_KEY)?;
    if ctx.json {
        println!("{}", serde_json::json!({ "cleared": true }));
    } else {
        println!("rate-limit cooldowns cleared");
    }
    Ok(0)
}

fn estimate(ctx: &mut Context, task: Option<&str>) -> Result<i32> {
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
                    human_f64(p.weighted_tokens),
                    human_f64(p.weighted_tokens * ModelTier::Fable.default_weight())
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
        let t = TierLedger { tier: ModelTier::Opus, period_budget: 100.0, period_weighted: 50.0, ..Default::default() };
        assert_eq!(tier_status(&t, 0.5, &limits, now), "ok");
        assert_eq!(tier_status(&t, 0.9, &limits, now), "under-paced");
        assert_eq!(tier_status(&t, 0.1, &limits, now), "over-paced");
        let t = TierLedger { period_weighted: 100.0, ..t };
        assert_eq!(tier_status(&t, 0.5, &limits, now), "exhausted");
        let t = TierLedger { period_budget: 0.0, ..t };
        assert_eq!(tier_status(&t, 0.5, &limits, now), "disabled");
        let mut limits = RateLimitState::default();
        limits.mark(ModelTier::Opus, now + Duration::minutes(5));
        assert_eq!(tier_status(&t, 0.5, &limits, now), "rate-limited");
    }
}
