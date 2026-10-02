//! `powerqueue config ...` — show, locate, edit and validate configuration.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use owo_colors::{OwoColorize, Stream, Style};

use crate::cli::{ConfigCommand, Context};
use crate::config::{Config, REPO_CONFIG_FILE, RepoOverrides};
use crate::paths::Paths;

/// Everything `config path` prints, in order.
pub fn path_entries(cfg: &Config, paths: &Paths) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("config", paths.config_file()),
        ("priority", cfg.priority_file(paths)),
        ("secrets_file", paths.secrets_file()),
        ("data", paths.data_dir.clone()),
        ("database", paths.database()),
        ("state", paths.state_dir.clone()),
        ("logs", paths.logs_dir()),
        ("tasks", paths.tasks_dir()),
        ("worktrees", if cfg.repo.path.is_empty() { paths.worktrees_dir() } else { cfg.worktree_root(paths) }),
        ("repo", cfg.repo_path()),
    ]
}

/// Problems found by `config validate`: config.toml problems plus a parse
/// error of the repo override file, if any.
pub fn validation_problems(cfg: &Config, repo_override: Option<&Path>) -> Vec<String> {
    let mut problems = cfg.validate();
    if let Some(file) = repo_override
        && file.exists()
    {
        match std::fs::read_to_string(file) {
            Ok(text) => {
                if let Err(e) = toml::from_str::<RepoOverrides>(&text) {
                    problems.push(format!("{}: {}", file.display(), e.message()));
                }
            }
            Err(e) => problems.push(format!("cannot read {}: {e}", file.display())),
        }
    }
    problems
}

/// The editor command from `$VISUAL`, then `$EDITOR`, falling back to `vi`.
pub fn editor_command() -> Vec<String> {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
            && let Some(parts) = shlex::split(&v)
            && !parts.is_empty()
        {
            return parts;
        }
    }
    vec!["vi".to_string()]
}

/// Open `file` in the user's editor and wait for it to exit.
pub fn open_in_editor(file: &Path) -> Result<()> {
    let cmd = editor_command();
    let status = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .arg(file)
        .status()
        .with_context(|| format!("launch editor `{}`", cmd.join(" ")))?;
    if !status.success() {
        bail!("editor `{}` exited with {status}", cmd.join(" "));
    }
    Ok(())
}

/// Handle `powerqueue config ...`.
pub fn run(ctx: &mut Context, cmd: ConfigCommand) -> Result<i32> {
    match cmd {
        ConfigCommand::Path => {
            let cfg = ctx.config_or_default()?.clone();
            let entries = path_entries(&cfg, &ctx.paths);
            if ctx.json {
                let map: serde_json::Map<String, serde_json::Value> =
                    entries.iter().map(|(k, v)| (k.to_string(), serde_json::Value::String(v.display().to_string()))).collect();
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for (k, v) in entries {
                    let exists = v.exists();
                    let shown = v.display().to_string();
                    println!(
                        "{:<13} {}",
                        k,
                        if exists || !ctx.color {
                            shown
                        } else {
                            format!("{shown} (missing)").if_supports_color(Stream::Stdout, |t| t.dimmed()).to_string()
                        }
                    );
                }
            }
            Ok(0)
        }
        ConfigCommand::Show => {
            super::status::ensure_initialised(ctx)?;
            let cfg = ctx.config_cloned()?;
            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&cfg)?);
                return Ok(0);
            }
            let override_file = cfg.repo_path().join(REPO_CONFIG_FILE);
            println!("# effective configuration (from {})", ctx.paths.config_file().display());
            if override_file.exists() {
                println!("# repository overrides applied from {}", override_file.display());
            } else {
                println!("# no repository overrides ({} not present)", override_file.display());
            }
            println!();
            print!("{}", cfg.to_toml()?);
            Ok(0)
        }
        ConfigCommand::Edit => {
            super::status::ensure_initialised(ctx)?;
            let file = ctx.paths.config_file();
            open_in_editor(&file)?;
            match Config::load(&ctx.paths) {
                Ok(cfg) => {
                    let problems = validation_problems(&cfg, Some(&cfg.repo_path().join(REPO_CONFIG_FILE)));
                    if problems.is_empty() {
                        println!("{} {} is valid", "ok".if_supports_color(Stream::Stdout, |t| t.green()), file.display());
                        Ok(0)
                    } else {
                        print_problems(&problems);
                        Ok(1)
                    }
                }
                Err(e) => {
                    crate::cli::output::print_error(&e);
                    Ok(1)
                }
            }
        }
        ConfigCommand::Validate => {
            super::status::ensure_initialised(ctx)?;
            let cfg = Config::load(&ctx.paths)?;
            let override_file = cfg.repo_path().join(REPO_CONFIG_FILE);
            let problems = validation_problems(&cfg, Some(&override_file));
            if ctx.json {
                println!("{}", serde_json::json!({ "ok": problems.is_empty(), "problems": problems }));
            } else if problems.is_empty() {
                println!(
                    "{} {} is valid",
                    "ok".if_supports_color(Stream::Stdout, |t| t.green()),
                    ctx.paths.config_file().display()
                );
                if override_file.exists() {
                    println!("{} {} is valid", "ok".if_supports_color(Stream::Stdout, |t| t.green()), override_file.display());
                }
            } else {
                print_problems(&problems);
            }
            Ok(if problems.is_empty() { 0 } else { 1 })
        }
    }
}

fn print_problems(problems: &[String]) {
    eprintln!(
        "{} {} problem(s):",
        "invalid".if_supports_color(Stream::Stderr, |t| t.style(Style::new().red().bold())),
        problems.len()
    );
    for p in problems {
        eprintln!("  - {p}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_entries_cover_everything() {
        let paths = Paths::rooted(Path::new("/tmp/pq"));
        let mut cfg = Config::default();
        cfg.repo.path = "/tmp/repo".into();
        let entries = path_entries(&cfg, &paths);
        let keys: Vec<_> = entries.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            vec!["config", "priority", "secrets_file", "data", "database", "state", "logs", "tasks", "worktrees", "repo"]
        );
        assert_eq!(entries[8].1, PathBuf::from("/tmp/pq/data/worktrees/repo"));
    }

    #[test]
    fn validation_includes_override_parse_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(REPO_CONFIG_FILE);
        std::fs::write(&file, "bogus = 1\n").unwrap();
        let mut cfg = Config::default();
        cfg.repo.path = dir.path().display().to_string();
        let problems = validation_problems(&cfg, Some(&file));
        assert!(problems.iter().any(|p| p.contains("bogus")), "{problems:?}");
        std::fs::write(&file, "setup = ['make']\n").unwrap();
        assert!(validation_problems(&cfg, Some(&file)).is_empty());
    }

    #[test]
    fn editor_fallback_is_vi() {
        let cmd = editor_command();
        assert!(!cmd.is_empty());
    }
}
