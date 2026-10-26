//! Focused project setup, preserving unrelated configuration and comments.

use anyhow::{Context as _, Result, bail};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::cli::{Context, ProjectCommand, output};
use crate::config::{Config, write_private};
use crate::worktree::Repo;

fn table<'a>(parent: &'a mut dyn toml_edit::TableLike, key: &str) -> Result<&'a mut dyn toml_edit::TableLike> {
    parent
        .entry(key)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .with_context(|| format!("config key `{key}` must be a table"))
}

/// Register a checkout and optional intake source, or list effective settings.
/// Invalid configuration/checkouts fail before writing. Existing source filters,
/// project settings, comments and machine settings are retained unless specified.
#[expect(
    clippy::as_conversions,
    reason = "This cast only coerces a mutable table reference to the table interface; no numeric conversion occurs."
)]
#[expect(clippy::too_many_lines, reason = "Keep the run workflow and its early exits together in execution order.")]
pub fn run(ctx: &mut Context, cmd: ProjectCommand) -> Result<i32> {
    match cmd {
        ProjectCommand::List => list(ctx),
        ProjectCommand::Register { id, path, github, token_env, trusted_approver } => {
            if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
                bail!("project ID must contain only ASCII letters, digits, - or _");
            }
            let checkout = path.canonicalize().with_context(|| format!("resolve checkout {}", path.display()))?;
            if !Repo::new(&checkout).is_repo() {
                bail!("{} is not a git repository", checkout.display());
            }
            let file = ctx.paths.config_file();
            let text = if file.exists() {
                std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?
            } else {
                // A GitHub/manual installation does not require a Linear account.
                "[linear]\nenabled = false\n".into()
            };
            let mut old = Config::from_toml(&text)?;
            let legacy = old.repo_path();
            if !old.repo.path.trim().is_empty() && legacy.exists() {
                old.apply_machine_repo_overrides(&legacy)?;
            }
            let mut doc: DocumentMut = text.parse().context("parse config.toml for project registration")?;
            super::config::migrate_legacy_document(&mut doc);
            let repo = if id == "default" && !old.repo.path.is_empty() {
                table(doc.as_table_mut(), "repo")?
            } else {
                let projects = table(doc.as_table_mut(), "projects")?;
                let project = table(projects, &id)?;
                table(project, "repo")?
            };
            repo.insert("path", value(checkout.to_string_lossy().as_ref()));
            if let Some(repository) = &github {
                let existing = old
                    .github_sources()
                    .into_iter()
                    .filter(|s| s.repository.eq_ignore_ascii_case(repository))
                    .collect::<Vec<_>>();
                if existing.len() > 1 {
                    bail!("GitHub source `{repository}` has conflicting assignments; remove duplicates first");
                }
                if let Some(source) = existing.first()
                    && source.execution_project != id
                {
                    bail!(
                        "GitHub source `{repository}` already maps to `{}`; use config edit for deliberate reassignment",
                        source.execution_project
                    );
                }
                let source = if old.github.repository.eq_ignore_ascii_case(repository) {
                    table(doc.as_table_mut(), "github")?
                } else {
                    // Config serialization can emit `github_sources = []`, and
                    // hand-written files may use an array of inline tables.
                    if let Some(array) = doc.get("github_sources").and_then(Item::as_array) {
                        let mut tables = toml_edit::ArrayOfTables::new();
                        for entry in array {
                            let inline = entry.as_inline_table().context("github_sources entries must be tables")?;
                            let mut table = Table::new();
                            for (key, value) in inline {
                                table.insert(key, Item::Value(value.clone()));
                            }
                            tables.push(table);
                        }
                        doc["github_sources"] = Item::ArrayOfTables(tables);
                    }
                    let sources = doc
                        .entry("github_sources")
                        .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::default()))
                        .as_array_of_tables_mut()
                        .context("github_sources must use [[github_sources]] tables")?;
                    let index = old.github_sources.iter().position(|s| s.repository.eq_ignore_ascii_case(repository));
                    let index = if let Some(i) = index {
                        i
                    } else {
                        sources.push(Table::new());
                        sources.len() - 1
                    };
                    sources.get_mut(index).context("locate GitHub source table")? as &mut dyn toml_edit::TableLike
                };
                source.insert("repository", value(repository));
                source.insert("execution_project", value(&id));
                if existing.is_empty() {
                    source.insert("enabled", value(true));
                    let mut labels = toml_edit::Array::new();
                    labels.push("agent:ready");
                    source.insert("required_labels", value(labels));
                }
                if let Some(name) = token_env {
                    source.insert("token_env", value(name));
                }
                if !trusted_approver.is_empty() {
                    let approvers: toml_edit::Array = trusted_approver.into_iter().collect();
                    source.insert("trusted_approvers", value(approvers));
                }
            }
            let updated = doc.to_string();
            let mut cfg = Config::from_toml(&updated)?;
            cfg.prompt.base_dir = Some(ctx.paths.config_dir.clone());
            if !cfg.repo.path.trim().is_empty() {
                cfg.apply_machine_repo_overrides(&cfg.repo_path())?;
            }
            cfg.ensure_valid()?;
            cfg.for_project(&id, &ctx.paths)?;
            // A setup command must not redirect already-bound work on a reload.
            for task in ctx.store()?.list_tasks()? {
                if task.execution_project == id {
                    cfg.for_task(&task, &ctx.paths)?;
                }
            }
            ctx.paths.ensure()?;
            write_private(&file, updated.as_bytes()).with_context(|| format!("write {}", file.display()))?;
            let note = super::config::apply_note(ctx)?;
            if ctx.json {
                println!(
                    "{}",
                    serde_json::json!({"execution_project": id, "checkout": checkout, "github": github, "note": note})
                );
            } else {
                println!("Registered {id}: {}", checkout.display());
                if let Some(repository) = github {
                    println!("GitHub {repository} → {id}");
                }
                println!("{note}\nInspect with `project list`; verify access with `github test` and preview with `github sync`.");
            }
            Ok(0)
        }
    }
}

fn list(ctx: &mut Context) -> Result<i32> {
    let cfg = ctx.config_cloned()?;
    let mut rows = Vec::new();
    let mut failed = false;
    for id in cfg.project_ids() {
        let sources: Vec<_> = cfg.github_sources().into_iter().filter(|s| s.execution_project == id).collect();
        match cfg.for_project(&id, &ctx.paths) {
            Ok(effective) => rows.push(serde_json::json!({
                "id": id, "checkout": effective.repo_path(), "worktree_root": effective.worktree_root(&ctx.paths),
                "sources": sources, "repo": effective.repo, "prompt": effective.prompt, "cleanup": effective.cleanup,
                "priority": effective.priority, "overrides": super::config::overrides_summary(&effective),
            })),
            Err(error) => {
                failed = true;
                rows.push(serde_json::json!({"id": id, "sources": sources, "error": format!("{error:#}")}));
            }
        }
    }
    if ctx.json {
        println!(
            "{}",
            serde_json::json!({"projects": rows, "max_concurrent": cfg.scheduler.max_concurrent, "budget": cfg.budget})
        );
    } else {
        let mut table = output::table();
        table.set_header(["PROJECT", "CHECKOUT", "WORKTREE ROOT", "GITHUB SOURCES"]);
        for row in rows {
            let sources = row["sources"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|s| {
                    format!(
                        "{}{}",
                        s["repository"].as_str().unwrap_or_default(),
                        if s["enabled"] == true { "" } else { " (disabled)" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            table.add_row([
                row["id"].as_str().unwrap_or_default(),
                row["checkout"].as_str().or(row["error"].as_str()).unwrap_or_default(),
                row["worktree_root"].as_str().unwrap_or_default(),
                &sources,
            ]);
        }
        println!("{table}\nShared concurrency: {}. Provider budgets apply to the whole queue.", cfg.scheduler.max_concurrent);
    }
    Ok(i32::from(failed))
}
