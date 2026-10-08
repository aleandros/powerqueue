//! `powerqueue github test|sync`.

use crate::cli::{Context, GitHubCommand, output};
use crate::github::{GitHubClient, apply_plan, plan_sync};
use crate::secrets::SecretKind;
use anyhow::{Result, bail};

/// Dispatch GitHub helpers. Errors include invalid config, credentials and API failures.
pub fn run(ctx: &mut Context, cmd: GitHubCommand) -> Result<i32> {
    let cfg = ctx.config_cloned()?.github;
    let mut validation = cfg.clone();
    validation.enabled = true; // Explicit helpers also work before daemon intake is enabled.
    let problems = validation.validate();
    if !problems.is_empty() {
        bail!("{}", problems.join("; "));
    }
    let client = GitHubClient::new(&cfg.endpoint, ctx.secrets().require(SecretKind::GitHubToken)?)?;
    let rt = super::runtime()?;
    match cmd {
        GitHubCommand::Test => {
            rt.block_on(client.test_repository(&cfg.repository))?;
            if ctx.json {
                println!("{}", serde_json::json!({"ok": true, "repository": cfg.repository}));
            } else {
                println!("GitHub repository {} is accessible", cfg.repository);
            }
        }
        GitHubCommand::Sync { apply } => {
            let store = ctx.store()?.clone();
            let plan = rt.block_on(plan_sync(&store, &cfg, &client))?;
            if apply {
                apply_plan(&store, &plan)?;
            }
            if ctx.json {
                println!("{}", serde_json::json!({"applied": apply, "fetched": plan.fetched, "changes": plan.changes}));
            } else {
                println!(
                    "{}: fetched {} issue(s), {} queue change(s).",
                    if apply { "Applied" } else { "Dry run" },
                    plan.fetched,
                    plan.changes.len()
                );
                let mut table = output::table();
                table.set_header(["ACTION", "KEY", "TITLE"]);
                for change in &plan.changes {
                    table.add_row([change.action.to_string(), change.task.key.clone(), output::truncate(&change.task.title, 60)]);
                }
                if !plan.changes.is_empty() {
                    println!("{table}");
                }
                if !apply {
                    println!("Re-run with --apply to sync the local queue.");
                }
            }
        }
    }
    Ok(0)
}
