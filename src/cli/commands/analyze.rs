//! Explicit analysis and retry; automatic mode may assign a repository, never source approval.

use anyhow::{Result, bail};

use crate::analysis::{AnalysisInput, AnalysisRecord, record_key};
use crate::cli::{Context, TaskRef, output};
use crate::secrets::SecretKind;

/// Analyze one existing task or display saved advice. Fails on invalid config or persistence.
/// Returns 3 for unresolved/stale results. Automatic mode may update unstarted repository assignment.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "Boolean parameters mirror independent CLI flags passed through command dispatch."
)]
pub fn run(ctx: &mut Context, task_ref: &TaskRef, show: bool) -> Result<i32> {
    let cfg = load_config(&ctx.paths)?;
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
        super::runtime()?.block_on(crate::analysis::client::analyze(&cfg, &input, jev_key.as_deref(), llm_key.as_deref()))?
    };
    // Reload after network I/O: source sync or a concurrent command may have changed the task.
    let current_cfg = load_config(&ctx.paths)?;
    let changed = if show {
        false
    } else {
        let (saved, changed) = store.save_and_apply_analysis(task.id, &record, &current_cfg)?;
        if !saved {
            bail!("a newer analysis already finished; inspect task analyze --show");
        }
        changed
    };
    let current = super::task::find_task(&store, &task.id.to_string())?;
    let assigned = changed && current.project_assignment.is_some();
    let stale = record.stale(&current_cfg, &current);
    let status = record.status(&current_cfg, &current);
    let resolved = !stale && record.proposal.as_ref().and_then(|p| p.project.as_ref()).is_some();
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "task": current.key, "status": status, "stale": stale,
                "repository_assigned": assigned, "execution_project": current.execution_project, "analysis": record,
            }))?
        );
    } else {
        output::print_analysis(&current.key, status, &record);
    }
    Ok(if resolved { 0 } else { 3 })
}

// Reload trusted machine settings and effective source restrictions after model I/O.
fn load_config(paths: &crate::paths::Paths) -> Result<crate::config::Config> {
    let mut cfg = crate::config::Config::load(paths)?;
    let repo = cfg.repo_path();
    if !cfg.repo.path.trim().is_empty() && (repo.exists() || !cfg.projects.is_empty()) {
        cfg.apply_machine_repo_overrides(&repo)?;
    }
    Ok(cfg)
}
