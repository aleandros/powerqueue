//! `powerqueue github test|sync` across configured sources.

use crate::cli::{Context, GitHubCommand, output};
use crate::github::{GitHubClient, apply_plan, plan_sync_routed};
use anyhow::{Result, bail};

/// Dispatch GitHub helpers. Valid sources continue after another source fails;
/// errors are reported per repository and produce a nonzero exit status.
#[expect(
    clippy::needless_pass_by_value,
    reason = "Command dispatch transfers ownership of parsed arguments to the selected handler."
)]
#[expect(clippy::too_many_lines, reason = "Keep the run workflow and its early exits together in execution order.")]
pub fn run(ctx: &mut Context, cmd: GitHubCommand) -> Result<i32> {
    let cfg = ctx.config_cloned()?;
    let mut problems = cfg.github_routing_problems();
    let sources = cfg.github_sources();
    if sources.is_empty() {
        bail!("configure github.repository or github_sources before using GitHub helpers");
    }
    for source in &sources {
        let mut validation = (*source).clone();
        validation.enabled = true; // Explicit helpers also preview disabled intake.
        problems.extend(validation.validate().into_iter().map(|p| format!("{}: {p}", source.repository)));
    }
    if !problems.is_empty() {
        bail!("{}", problems.join("; "));
    }
    let rt = super::runtime()?;
    let mut errors = Vec::new();
    let mut results = Vec::new();
    let mut changes = Vec::new();
    let mut problems = Vec::new();
    let mut fetched = 0;
    let apply = matches!(cmd, GitHubCommand::Sync { apply: true });
    for source in sources {
        let result: Result<()> = (|| {
            let (token, _) = source.token(ctx.secrets())?;
            let client = GitHubClient::new(&source.endpoint, token)?;
            match cmd {
                GitHubCommand::Test => {
                    rt.block_on(client.test_repository(&source.repository))?;
                }
                GitHubCommand::Sync { .. } => {
                    let store = ctx.store()?.clone();
                    // Explicit helpers preview/import disabled sources too. Select their
                    // configured route; the daemon still requires the actual source enabled.
                    let mut preview_config = cfg.clone();
                    if preview_config.github.repository.eq_ignore_ascii_case(&source.repository) {
                        preview_config.github.enabled = true;
                    }
                    for configured in &mut preview_config.github_sources {
                        if configured.repository.eq_ignore_ascii_case(&source.repository) {
                            configured.enabled = true;
                        }
                    }
                    let plan = rt.block_on(plan_sync_routed(&store, source, &client, &preview_config))?;
                    if apply {
                        apply_plan(&store, &plan)?;
                    }
                    fetched += plan.fetched;
                    changes.extend(plan.changes);
                    problems.extend(plan.problems);
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                results.push(serde_json::json!({"repository": source.repository, "execution_project": source.execution_project}));
            }
            Err(error) => errors.push(serde_json::json!({"repository": source.repository, "error": format!("{error:#}")})),
        }
    }
    if ctx.json {
        match cmd {
            GitHubCommand::Test => println!(
                "{}",
                serde_json::json!({"ok": errors.is_empty(), "repository": cfg.github.repository,
                "sources": results, "errors": errors})
            ),
            GitHubCommand::Sync { .. } => println!(
                "{}",
                serde_json::json!({"applied": apply, "fetched": fetched,
                "changes": changes, "sources": results, "errors": errors, "problems": problems})
            ),
        }
    } else {
        match cmd {
            GitHubCommand::Test => {
                for source in &results {
                    println!(
                        "GitHub repository {} is accessible (project {})",
                        source["repository"].as_str().unwrap_or_default(),
                        source["execution_project"].as_str().unwrap_or_default()
                    );
                }
            }
            GitHubCommand::Sync { .. } => {
                for problem in &problems {
                    eprintln!("warning: {}: {}", problem.key, problem.reason);
                }
                println!(
                    "{}: fetched {} issue(s), {} queue change(s).",
                    if apply { "Applied" } else { "Dry run" },
                    fetched,
                    changes.len()
                );
                let mut table = output::table();
                table.set_header(["ACTION", "KEY", "PROJECT", "TITLE", "ROUTING", "ADMISSION"]);
                for change in &changes {
                    table.add_row([
                        change.action.to_string(),
                        change.task.key.clone(),
                        change.task.execution_project.clone(),
                        output::truncate(&change.task.title, 60),
                        change.task.routing.as_ref().map(crate::routing::RoutingDecision::explanation).unwrap_or_default(),
                        change.task.github_admission.as_ref().map(|a| a.reason.clone()).unwrap_or_default(),
                    ]);
                }
                if !changes.is_empty() {
                    println!("{table}");
                }
                if !apply {
                    println!("Re-run with --apply to sync the local queue.");
                }
            }
        }
        for error in &errors {
            eprintln!(
                "GitHub {}: {}",
                error["repository"].as_str().unwrap_or_default(),
                error["error"].as_str().unwrap_or_default()
            );
        }
    }
    Ok(i32::from(!errors.is_empty()))
}
