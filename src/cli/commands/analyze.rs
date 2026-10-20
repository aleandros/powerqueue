//! Explicit, optional analysis. Human routing and source approval remain separate commands.

use anyhow::{Result, bail};

use crate::analysis::{AnalysisInput, AnalysisRecord, record_key};
use crate::cli::{Context, TaskRef, output};
use crate::secrets::SecretKind;

/// Analyze one existing task or display saved advice. Fails on invalid config or persistence.
/// Returns 3 for unresolved/stale results; no task row or source system is mutated.
pub fn run(ctx: &mut Context, task_ref: &TaskRef, show: bool) -> Result<i32> {
    let cfg = crate::config::Config::load(&ctx.paths)?;
    let store = ctx.store()?.clone();
    let task = super::task::find_task(&store, &task_ref.task)?;
    let record = if show {
        store
            .kv_get::<AnalysisRecord>(&record_key(task.id))?
            .ok_or_else(|| anyhow::anyhow!("no saved analysis for {}; run task analyze first", task.key))?
    } else {
        if !cfg.analysis.enabled {
            bail!("analysis is disabled; configure analysis.enabled and trusted project descriptions first");
        }
        let input = AnalysisInput::new(&cfg, &task)?;
        let jev_key = if cfg.analysis.jev_first { ctx.secrets().get(SecretKind::JevApiKey)? } else { None };
        let llm_key = if cfg.analysis.llm.is_some() { ctx.secrets().get(SecretKind::AnalysisApiKey)? } else { None };
        let record =
            super::runtime()?.block_on(crate::analysis::client::analyze(&cfg, &input, jev_key.as_deref(), llm_key.as_deref()))?;
        if !store.save_analysis(task.id, &record)? {
            bail!("a newer analysis already finished; inspect task analyze --show");
        }
        record
    };
    // Reload after network I/O: source sync or a concurrent command may have changed the task.
    let current = super::task::find_task(&store, &task.id.to_string())?;
    let current_cfg = crate::config::Config::load(&ctx.paths)?;
    let stale = record.stale(&current_cfg, &current);
    let status = record.status(&current_cfg, &current);
    let resolved = !stale && record.proposal.as_ref().and_then(|p| p.project.as_ref()).is_some();
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "task": current.key, "status": status, "stale": stale,
                "requires_human_approval": true, "analysis": record,
            }))?
        );
    } else {
        output::print_analysis(&current.key, status, &record);
    }
    Ok(if resolved { 0 } else { 3 })
}
