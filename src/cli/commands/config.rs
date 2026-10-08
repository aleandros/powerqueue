//! `powerqueue config ...` — show, locate, edit, validate and change single
//! keys of the configuration.
//!
//! `get`/`set`/`unset` address keys by dotted path (`claude.permission_mode`,
//! `budget.providers.claude.models.fable.share`). Edits go through
//! `toml_edit` so comments and formatting in `config.toml` survive, and
//! nothing is written unless the result parses as a [`Config`] and passes
//! [`Config::validate`]. Legacy budget keys (`budget.models.fable.share`,
//! `budget.period_hours`) are rewritten to their `budget.providers.claude`
//! location, and a file still in the old shape is migrated in place before
//! the edit so old and new keys never coexist.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use owo_colors::{OwoColorize, Stream, Style};
use toml_edit::{DocumentMut, Item, Table};

use crate::cli::commands::status::daemon_status;
use crate::cli::{ConfigCommand, Context};
use crate::config::{Config, LEGACY_BUDGET_KEYS, REPO_CONFIG_FILE, rewrite_legacy_key, write_private};
use crate::domain::DaemonCommand;
use crate::paths::Paths;

/// Split a dotted key into segments, rejecting empty ones.
fn key_segments(key: &str) -> Result<Vec<&str>> {
    let segs: Vec<&str> = key.split('.').collect();
    if key.trim().is_empty() || segs.iter().any(|s| s.trim().is_empty()) {
        bail!("`{key}` is not a dotted key such as `claude.permission_mode`");
    }
    Ok(segs)
}

/// The key to actually edit: legacy budget keys map to their new location.
/// Returns the effective key and the note to show when it was rewritten.
pub fn effective_key(key: &str) -> (String, Option<String>) {
    match rewrite_legacy_key(key) {
        Some(new) => {
            let note = format!("`{key}` now lives at `{new}`; edited that key instead");
            (new, Some(note))
        }
        None => (key.to_string(), None),
    }
}

/// Move the legacy flat `[budget]` keys and `[budget.models.*]` of a
/// `toml_edit` document under `budget.providers.claude`, keeping the rest
/// of the document (comments included) intact. Returns the moved keys.
pub fn migrate_legacy_document(doc: &mut DocumentMut) -> Vec<String> {
    let mut moved = Vec::new();
    let Some(budget) = doc.get_mut("budget").and_then(|b| b.as_table_like_mut()) else { return moved };
    let mut taken: Vec<(String, Item)> = Vec::new();
    for key in LEGACY_BUDGET_KEYS.iter().chain(["models"].iter()) {
        if let Some(item) = budget.remove(key) {
            taken.push((key.to_string(), item));
        }
    }
    if taken.is_empty() {
        return moved;
    }
    let providers = budget.entry("providers").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    let Some(providers) = providers.as_table_like_mut() else { return moved };
    let claude = providers.entry("claude").or_insert_with(|| Item::Table(Table::new()));
    let Some(claude) = claude.as_table_like_mut() else { return moved };
    for (key, item) in taken {
        if claude.get(&key).is_none() {
            // Inline `models.fable = {...}` would be unusual; keep whatever shape the item has.
            claude.insert(&key, item);
            moved.push(format!("budget.{key} -> budget.providers.claude.{key}"));
        } else {
            moved.push(format!("budget.{key} dropped (budget.providers.claude.{key} already set)"));
        }
    }
    moved
}

/// Parse `raw` as a TOML value (`3`, `true`, `0.25`, `"x"`, `["ENG", "OPS"]`,
/// `{ a = 1 }`); anything that does not parse is taken as a plain string, so
/// `auto` and `In Progress` work without quotes. Date-times are kept as
/// strings too, because every timestamp in the configuration is a string
/// (`budget.providers.claude.period_anchor`).
pub fn parse_toml_value(raw: &str) -> toml_edit::Value {
    match raw.trim().parse::<toml_edit::Value>() {
        Ok(v) if v.is_datetime() => toml_edit::Value::from(raw.trim()),
        Ok(v) if !v.is_str() || raw.trim().starts_with(['"', '\'']) => v,
        _ => toml_edit::Value::from(raw),
    }
}

/// Set `key` to the TOML value `raw` in `text` (the contents of
/// `config.toml`), creating intermediate tables as needed. Returns the new
/// contents. Fails when the document does not parse, when the result is not a
/// valid [`Config`] (unknown key, wrong type) or when [`Config::validate`]
/// reports problems.
pub fn set_in_toml(text: &str, key: &str, raw: &str) -> Result<String> {
    let (key, _) = effective_key(key);
    let key = key.as_str();
    let segs = key_segments(key)?;
    let mut doc: DocumentMut = text.parse().context("config.toml does not parse")?;
    migrate_legacy_document(&mut doc);
    let value = parse_toml_value(raw);
    let (last, parents) = segs.split_last().expect("key_segments returns at least one segment");
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    for seg in parents {
        let item = table.entry(seg).or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        });
        table = item.as_table_like_mut().ok_or_else(|| anyhow!("`{seg}` in `{key}` is not a table"))?;
    }
    table.insert(last, Item::Value(value));
    let out = doc.to_string();
    check_edited(&out, key)?;
    Ok(out)
}

/// Remove `key` from `text` so its default applies again. Returns the new
/// contents and whether the key was present. Fails like [`set_in_toml`] when
/// the result is invalid (for example after removing `repo.path`).
pub fn unset_in_toml(text: &str, key: &str) -> Result<(String, bool)> {
    let (key, _) = effective_key(key);
    let key = key.as_str();
    let segs = key_segments(key)?;
    let mut doc: DocumentMut = text.parse().context("config.toml does not parse")?;
    let migrated = !migrate_legacy_document(&mut doc).is_empty();
    let (last, parents) = segs.split_last().expect("key_segments returns at least one segment");
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    for seg in parents {
        match table.get_mut(seg).and_then(|i| i.as_table_like_mut()) {
            Some(t) => table = t,
            None => return Ok((if migrated { doc.to_string() } else { text.to_string() }, false)),
        }
    }
    let present = table.remove(last).is_some();
    let out = doc.to_string();
    check_edited(&out, key)?;
    Ok((out, present))
}

/// Parse the edited text as a [`Config`] and validate it, naming `key` in errors.
fn check_edited(text: &str, key: &str) -> Result<()> {
    let cfg = Config::from_toml(text).map_err(|e| anyhow!("`{key}`: {}", e.to_string().trim()))?;
    let problems = cfg.validate();
    if !problems.is_empty() {
        bail!("`{key}` would leave config.toml invalid:\n  - {}", problems.join("\n  - "));
    }
    Ok(())
}

/// The effective value at `key` (file plus defaults, no repo overrides) as
/// JSON; `None` when the key names nothing in the configuration or is unset.
pub fn get_value(cfg: &Config, key: &str) -> Result<Option<serde_json::Value>> {
    let (key, _) = effective_key(key);
    let segs = key_segments(&key)?;
    let root = serde_json::to_value(cfg)?;
    let mut cur = &root;
    for seg in segs {
        match cur.get(seg) {
            Some(v) if !v.is_null() => cur = v,
            _ => return Ok(None),
        }
    }
    Ok(Some(cur.clone()))
}

/// Render a JSON value the way it would appear in `config.toml`.
pub fn toml_display(value: &serde_json::Value) -> Result<String> {
    use serde::Deserialize as _;
    let v = toml::Value::deserialize(value.clone()).map_err(|e| anyhow!("cannot render as TOML: {e}"))?;
    Ok(match v {
        toml::Value::Table(t) => toml::to_string(&t)?.trim_end().to_string(),
        other => other.to_string(),
    })
}

/// Tell the user whether a changed setting reaches the daemon: a live daemon
/// is asked to reload (new sessions use the new values; running sessions and
/// `logging.*` do not), otherwise the next `powerqueue run` picks it up.
pub fn apply_note(ctx: &mut Context) -> Result<String> {
    let store = ctx.store()?;
    let daemon = daemon_status(store, chrono::Utc::now())?;
    Ok(if daemon.alive {
        store.enqueue_command(&DaemonCommand::Reload).context("queue reload command")?;
        "daemon asked to reload: new sessions use the new settings; running sessions keep theirs and `logging.*` needs a restart"
            .to_string()
    } else {
        "no daemon running; the next `powerqueue run` uses the new settings".to_string()
    })
}

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
        ("repo_config", cfg.repo_path().join(REPO_CONFIG_FILE)),
    ]
}

/// Problems found by `config validate`: the repository's `.powerqueue.toml`
/// is applied first (from wherever `repo.overrides_from` says), so a bad
/// value the repo sets (`[scheduler] max_concurrent = 0`) is reported too;
/// a file that cannot be read or parsed is one problem and the global
/// config is validated alone. Returns the config that was validated.
pub fn validation_problems(cfg: &Config) -> (Config, Vec<String>) {
    let mut merged = cfg.clone();
    let mut problems = Vec::new();
    let repo = cfg.repo_path();
    if !cfg.repo.path.trim().is_empty()
        && repo.exists()
        && let Err(e) = merged.apply_repo_overrides(&repo)
    {
        problems.push(format!("{e:#}"));
        merged = cfg.clone();
    }
    problems.extend(merged.validate());
    (merged, problems)
}

/// One line saying what `.powerqueue.toml` contributed, for `config show`
/// and `config validate`.
pub fn overrides_summary(cfg: &Config) -> String {
    match &cfg.overrides.file {
        Some(file) if cfg.overrides.keys.is_empty() => {
            format!("repository overrides from {} ({}): no keys set", file.display(), cfg.overrides.origin())
        }
        Some(file) => format!(
            "repository overrides from {} ({}): {}",
            file.display(),
            cfg.overrides.origin(),
            cfg.overrides.keys.join(", ")
        ),
        None => format!(
            "no repository overrides ({} not present in the {})",
            cfg.repo_path().join(REPO_CONFIG_FILE).display(),
            match cfg.repo.overrides_from {
                crate::config::OverridesSource::WorkingTree => "working tree".to_string(),
                crate::config::OverridesSource::DefaultBranch => "default branch".to_string(),
            }
        ),
    }
}

/// Mark every `key = value` line of a pretty-printed config that came from
/// `.powerqueue.toml` with a trailing comment. Table headers (`[linear]`)
/// set the prefix; only the first line of a multi-line value is marked.
pub fn annotate_overrides(toml: &str, keys: &[String]) -> String {
    let mut table = String::new();
    let mut out = String::with_capacity(toml.len() + keys.len() * 24);
    for line in toml.lines() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            table = name.trim_matches('[').trim_matches(']').to_string();
            out.push_str(line);
        } else if let Some((key, _)) = trimmed.split_once('=')
            && !trimmed.starts_with('#')
            && key.trim().chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '"')
        {
            let key = key.trim().trim_matches('"');
            let full = if table.is_empty() { key.to_string() } else { format!("{table}.{key}") };
            out.push_str(line);
            if keys.iter().any(|k| k == &full) {
                out.push_str("  # .powerqueue.toml");
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
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
                let mut value = serde_json::to_value(&cfg)?;
                if let serde_json::Value::Object(map) = &mut value {
                    map.insert(
                        "repo_overrides".to_string(),
                        serde_json::json!({
                            "file": cfg.overrides.file,
                            "source": cfg.repo.overrides_from,
                            "rev": cfg.overrides.rev,
                            "keys": cfg.overrides.keys,
                        }),
                    );
                }
                println!("{}", serde_json::to_string_pretty(&value)?);
                return Ok(0);
            }
            println!("# effective configuration (from {})", ctx.paths.config_file().display());
            println!("# {}", overrides_summary(&cfg));
            if !cfg.overrides.keys.is_empty() {
                println!("# keys marked `# .powerqueue.toml` come from the repository");
            }
            println!();
            print!("{}", annotate_overrides(&cfg.to_toml()?, &cfg.overrides.keys));
            Ok(0)
        }
        ConfigCommand::Get { key } => {
            super::status::ensure_initialised(ctx)?;
            let cfg = Config::load(&ctx.paths)?;
            match get_value(&cfg, &key)? {
                Some(v) if ctx.json => {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    Ok(0)
                }
                Some(v) => {
                    println!("{}", toml_display(&v)?);
                    Ok(0)
                }
                None if ctx.json => {
                    println!("null");
                    Ok(1)
                }
                None => {
                    eprintln!("{key} is not set (and has no default); see `powerqueue config show` for the known keys");
                    Ok(1)
                }
            }
        }
        ConfigCommand::Set { key, value } => {
            super::status::ensure_initialised(ctx)?;
            let file = ctx.paths.config_file();
            let text = std::fs::read_to_string(&file).with_context(|| format!("cannot read {}", file.display()))?;
            let (key, note) = effective_key(&key);
            if let Some(n) = &note
                && !ctx.json
            {
                eprintln!("{n}");
            }
            let edited = match set_in_toml(&text, &key, &value) {
                Ok(t) => t,
                Err(e) => {
                    if ctx.json {
                        println!("{}", serde_json::json!({ "ok": false, "key": key, "error": format!("{e:#}") }));
                    } else {
                        print_problems(&[format!("{e:#}")]);
                        eprintln!("nothing written");
                    }
                    return Ok(1);
                }
            };
            write_private(&file, edited.as_bytes()).with_context(|| format!("write {}", file.display()))?;
            let cfg = Config::load(&ctx.paths)?;
            let shown = get_value(&cfg, &key)?.unwrap_or(serde_json::Value::Null);
            let note = apply_note(ctx)?;
            if ctx.json {
                println!("{}", serde_json::json!({ "ok": true, "key": key, "value": shown, "note": note }));
            } else {
                println!("{} {key} = {}", "set".if_supports_color(Stream::Stdout, |t| t.green()), toml_display(&shown)?);
                println!("{note}");
            }
            Ok(0)
        }
        ConfigCommand::Unset { key } => {
            super::status::ensure_initialised(ctx)?;
            let file = ctx.paths.config_file();
            let text = std::fs::read_to_string(&file).with_context(|| format!("cannot read {}", file.display()))?;
            let (key, note) = effective_key(&key);
            if let Some(n) = &note
                && !ctx.json
            {
                eprintln!("{n}");
            }
            let (edited, present) = match unset_in_toml(&text, &key) {
                Ok(r) => r,
                Err(e) => {
                    if ctx.json {
                        println!("{}", serde_json::json!({ "ok": false, "key": key, "error": format!("{e:#}") }));
                    } else {
                        print_problems(&[format!("{e:#}")]);
                        eprintln!("nothing written");
                    }
                    return Ok(1);
                }
            };
            if !present {
                if ctx.json {
                    println!("{}", serde_json::json!({ "ok": true, "key": key, "changed": false }));
                } else {
                    println!("{key} is not set in {}; the default already applies", file.display());
                }
                return Ok(0);
            }
            write_private(&file, edited.as_bytes()).with_context(|| format!("write {}", file.display()))?;
            let cfg = Config::load(&ctx.paths)?;
            let shown = get_value(&cfg, &key)?.unwrap_or(serde_json::Value::Null);
            let note = apply_note(ctx)?;
            if ctx.json {
                println!("{}", serde_json::json!({ "ok": true, "key": key, "changed": true, "value": shown, "note": note }));
            } else {
                println!(
                    "{} {key} (now {})",
                    "unset".if_supports_color(Stream::Stdout, |t| t.green()),
                    if shown.is_null() { "not set".to_string() } else { toml_display(&shown)? }
                );
                println!("{note}");
            }
            Ok(0)
        }
        ConfigCommand::Edit => {
            super::status::ensure_initialised(ctx)?;
            let file = ctx.paths.config_file();
            open_in_editor(&file)?;
            match Config::load(&ctx.paths) {
                Ok(cfg) => {
                    let (_, problems) = validation_problems(&cfg);
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
        ConfigCommand::Validate { file } => {
            let (cfg, shown) = match file {
                Some(file) => {
                    if !file.exists() {
                        bail!("{} does not exist", file.display());
                    }
                    let text = std::fs::read_to_string(&file).with_context(|| format!("cannot read {}", file.display()))?;
                    match Config::from_toml(&text) {
                        Ok(cfg) => (cfg, file),
                        Err(e) => {
                            let problem = format!("{}: {e:#}", file.display());
                            if ctx.json {
                                println!("{}", serde_json::json!({ "ok": false, "problems": [problem] }));
                            } else {
                                print_problems(std::slice::from_ref(&problem));
                            }
                            return Ok(1);
                        }
                    }
                }
                None => {
                    super::status::ensure_initialised(ctx)?;
                    (Config::load(&ctx.paths)?, ctx.paths.config_file())
                }
            };
            let (merged, problems) = validation_problems(&cfg);
            if ctx.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": problems.is_empty(), "file": shown, "problems": problems,
                        "repo_overrides": {
                            "file": merged.overrides.file, "source": merged.repo.overrides_from,
                            "rev": merged.overrides.rev, "keys": merged.overrides.keys,
                        },
                    })
                );
            } else if problems.is_empty() {
                println!("{} {} is valid", "ok".if_supports_color(Stream::Stdout, |t| t.green()), shown.display());
                if let Some(file) = &merged.overrides.file {
                    println!(
                        "{} {} ({}) is valid and sets {}",
                        "ok".if_supports_color(Stream::Stdout, |t| t.green()),
                        file.display(),
                        merged.overrides.origin(),
                        if merged.overrides.keys.is_empty() { "no keys".to_string() } else { merged.overrides.keys.join(", ") }
                    );
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
            vec![
                "config",
                "priority",
                "secrets_file",
                "data",
                "database",
                "state",
                "logs",
                "tasks",
                "worktrees",
                "repo",
                "repo_config"
            ]
        );
        assert_eq!(entries[8].1, PathBuf::from("/tmp/pq/data/worktrees/repo"));
    }

    #[test]
    fn validation_includes_override_parse_errors_and_merged_values() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(REPO_CONFIG_FILE);
        std::fs::write(&file, "bogus = 1\n").unwrap();
        let mut cfg = Config::default();
        cfg.repo.path = dir.path().display().to_string();
        let (_, problems) = validation_problems(&cfg);
        assert!(problems.iter().any(|p| p.contains("bogus")), "{problems:?}");
        std::fs::write(&file, "setup = ['make']\n").unwrap();
        let (merged, problems) = validation_problems(&cfg);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(merged.overrides.keys, vec!["repo.setup".to_string()]);
        // A bad value set by the repo is a problem of the merged config.
        std::fs::write(&file, "[scheduler]\nmax_concurrent = 0\n").unwrap();
        let (_, problems) = validation_problems(&cfg);
        assert!(problems.iter().any(|p| p.contains("scheduler.max_concurrent")), "{problems:?}");
    }

    #[test]
    fn annotate_marks_only_repo_keys() {
        let toml = "[repo]\npath = \"/r\"\nsetup = [\n    \"make\",\n]\n\n[linear]\nexcluded_labels = [\"x\"]\ncycle = \"any\"\n";
        let keys = vec!["repo.setup".to_string(), "linear.cycle".to_string()];
        let out = annotate_overrides(toml, &keys);
        assert!(out.contains("setup = [  # .powerqueue.toml\n"), "{out}");
        assert!(out.contains("cycle = \"any\"  # .powerqueue.toml\n"), "{out}");
        assert!(out.contains("path = \"/r\"\n"), "{out}");
        assert!(out.contains("excluded_labels = [\"x\"]\n"), "{out}");
        assert_eq!(annotate_overrides(toml, &[]), toml);
    }

    #[test]
    fn editor_fallback_is_vi() {
        let cmd = editor_command();
        assert!(!cmd.is_empty());
    }

    const BASE: &str =
        "# header comment\n[repo]\npath = \"/tmp/repo\" # keep me\n\n[claude]\npermission_mode = \"acceptEdits\"\n";

    #[test]
    fn set_scalar_keeps_comments() {
        let out = set_in_toml(BASE, "claude.permission_mode", "auto").unwrap();
        assert!(out.starts_with("# header comment\n"), "{out}");
        assert!(out.contains("path = \"/tmp/repo\" # keep me"), "{out}");
        assert!(out.contains("permission_mode = \"auto\""), "{out}");
        let cfg = Config::from_toml(&out).unwrap();
        assert_eq!(cfg.claude.permission_mode, "auto");

        let out = set_in_toml(BASE, "scheduler.max_concurrent", "3").unwrap();
        assert!(out.contains("[scheduler]\nmax_concurrent = 3"), "{out}");
        assert_eq!(Config::from_toml(&out).unwrap().scheduler.max_concurrent, 3);

        // Quoted strings and bare strings with spaces both work.
        let out = set_in_toml(BASE, "linear.in_progress_state", "In Progress").unwrap();
        assert_eq!(Config::from_toml(&out).unwrap().linear.in_progress_state.as_deref(), Some("In Progress"));
        let out = set_in_toml(BASE, "linear.in_progress_state", "\"Doing\"").unwrap();
        assert_eq!(Config::from_toml(&out).unwrap().linear.in_progress_state.as_deref(), Some("Doing"));

        // `post_comments` takes a boolean or "questions".
        let out = set_in_toml(BASE, "linear.post_comments", "questions").unwrap();
        assert_eq!(Config::from_toml(&out).unwrap().linear.post_comments, crate::config::PostComments::Questions);
        let out = set_in_toml(BASE, "linear.post_comments", "false").unwrap();
        assert_eq!(Config::from_toml(&out).unwrap().linear.post_comments, crate::config::PostComments::Off);
    }

    #[test]
    fn set_array_and_nested_table() {
        let out = set_in_toml(BASE, "linear.team_keys", "[\"ENG\", \"OPS\"]").unwrap();
        assert_eq!(Config::from_toml(&out).unwrap().linear.team_keys, vec!["ENG".to_string(), "OPS".to_string()]);

        let fable = crate::domain::ModelTier::fable();
        let out = set_in_toml(BASE, "budget.providers.claude.models.fable.share", "0.1").unwrap();
        assert!(out.contains("[budget.providers.claude.models.fable]\nshare = 0.1"), "{out}");
        assert!(!out.contains("[budget]\n"), "intermediate tables stay implicit: {out}");
        let cfg = Config::from_toml(&out).unwrap();
        assert_eq!(cfg.budget.providers.claude.models[&fable].share, 0.1);
        // Untouched fields of the partially specified table keep the shipped
        // defaults, and the other shipped models stay.
        assert_eq!(cfg.budget.providers.claude.models[&fable].weight, 5.0);
        assert_eq!(cfg.budget.providers.claude.models.len(), 4);

        let full = Config::default().to_toml().unwrap().replace("path = \"\"", "path = \"/tmp/repo\"");
        let out = set_in_toml(&full, "budget.providers.claude.models.fable.share", "0.1").unwrap();
        let cfg = Config::from_toml(&out).unwrap();
        assert_eq!(cfg.budget.providers.claude.models[&fable].share, 0.1);
        assert_eq!(cfg.budget.providers.claude.models[&fable].weight, 5.0);
        assert_eq!(cfg.budget.providers.claude.models.len(), 4);
    }

    const LEGACY: &str = "# header\n[repo]\npath = \"/tmp/repo\"\n\n[budget]\nperiod_hours = 100 # old\ndefault_model = \"haiku\"\n\n[budget.models.fable]\nshare = 0.2\nweight = 4.0\n\n[budget.models.opus]\nshare = 0.3\n";

    #[test]
    fn legacy_keys_are_rewritten_and_the_file_migrated() {
        let fable = crate::domain::ModelTier::fable();
        let (key, note) = effective_key("budget.models.fable.share");
        assert_eq!(key, "budget.providers.claude.models.fable.share");
        assert!(note.unwrap().contains("now lives at"));
        assert_eq!(effective_key("claude.binary"), ("claude.binary".to_string(), None));

        let out = set_in_toml(LEGACY, "budget.models.fable.share", "0.3").unwrap();
        assert!(out.starts_with("# header\n"), "{out}");
        assert!(!out.contains("[budget.models.fable]"), "old table is gone: {out}");
        assert!(out.contains("[budget.providers.claude.models.fable]"), "{out}");
        assert!(out.contains("period_hours = 100"), "{out}");
        assert!(out.contains("default_model = \"haiku\""), "shared keys stay in [budget]: {out}");
        let cfg = Config::from_toml(&out).unwrap();
        assert!(cfg.budget.migrated_keys.is_empty(), "the written file is in the new shape: {:?}", cfg.budget.migrated_keys);
        assert_eq!(cfg.budget.providers.claude.period_hours, 100);
        assert_eq!(cfg.budget.providers.claude.models[&fable].share, 0.3);
        assert_eq!(cfg.budget.providers.claude.models[&fable].weight, 4.0);
        assert_eq!(cfg.budget.providers.claude.models[&crate::domain::ModelTier::opus()].share, 0.3);
        assert_eq!(cfg.budget.default_model, crate::domain::ModelTier::haiku());

        let out = set_in_toml(LEGACY, "budget.period_anchor", "2026-10-06T07:00:00Z").unwrap();
        let cfg = Config::from_toml(&out).unwrap();
        assert_eq!(cfg.budget.providers.claude.period_anchor.as_deref(), Some("2026-10-06T07:00:00Z"));
        assert_eq!(cfg.budget.providers.claude.period_hours, 100);

        let (out, present) = unset_in_toml(LEGACY, "budget.models.opus.share").unwrap();
        assert!(present);
        assert!(!out.contains("[budget.models.opus]"), "{out}");
        let cfg = Config::from_toml(&out).unwrap();
        assert_eq!(cfg.budget.providers.claude.models[&crate::domain::ModelTier::opus()].share, 0.35);

        let cfg = Config::from_toml(LEGACY).unwrap();
        let v = get_value(&cfg, "budget.models.fable.share").unwrap().unwrap();
        assert_eq!(toml_display(&v).unwrap(), "0.2");
    }

    #[test]
    fn set_rejects_invalid_values_and_keys() {
        let err = set_in_toml(BASE, "claude.permission_mode", "bogus").unwrap_err().to_string();
        assert!(err.contains("permission_mode"), "{err}");
        let err = set_in_toml(BASE, "scheduler.max_concurrent", "0").unwrap_err().to_string();
        assert!(err.contains("max_concurrent must be >= 1"), "{err}");
        let err = set_in_toml(BASE, "scheduler.max_concurrent", "three").unwrap_err().to_string();
        assert!(err.contains("max_concurrent"), "{err}");
        let err = set_in_toml(BASE, "claude.permision_mode", "auto").unwrap_err().to_string();
        assert!(err.contains("permision_mode"), "{err}");
        assert!(set_in_toml(BASE, "", "1").is_err());
        assert!(set_in_toml(BASE, "claude..x", "1").is_err());
        let err = set_in_toml(BASE, "repo.path.deeper", "1").unwrap_err().to_string();
        assert!(err.contains("not a table"), "{err}");
    }

    #[test]
    fn unset_removes_key_and_validates() {
        let (out, present) = unset_in_toml(BASE, "claude.permission_mode").unwrap();
        assert!(present);
        assert!(!out.contains("permission_mode"), "{out}");
        assert!(out.contains("# header comment"), "{out}");
        assert_eq!(Config::from_toml(&out).unwrap().claude.permission_mode, "acceptEdits");

        let (out, present) = unset_in_toml(BASE, "scheduler.max_concurrent").unwrap();
        assert!(!present);
        assert_eq!(out, BASE);

        let err = unset_in_toml(BASE, "repo.path").unwrap_err().to_string();
        assert!(err.contains("repo.path is empty"), "{err}");
    }

    #[test]
    fn get_value_and_display() {
        let cfg = Config::from_toml(BASE).unwrap();
        let v = get_value(&cfg, "claude.permission_mode").unwrap().unwrap();
        assert_eq!(toml_display(&v).unwrap(), "\"acceptEdits\"");
        let v = get_value(&cfg, "scheduler.max_concurrent").unwrap().unwrap();
        assert_eq!(toml_display(&v).unwrap(), "2");
        let v = get_value(&cfg, "linear.queued_states").unwrap().unwrap();
        assert_eq!(toml_display(&v).unwrap(), "[\"Todo\"]");
        let v = get_value(&cfg, "budget.providers.claude.models.fable.share").unwrap().unwrap();
        assert_eq!(toml_display(&v).unwrap(), "0.25");
        let v = get_value(&cfg, "budget.providers.claude.models.fable").unwrap().unwrap();
        assert!(toml_display(&v).unwrap().contains("share = 0.25"));
        let v = get_value(&cfg, "budget.providers.codex.enabled").unwrap().unwrap();
        assert_eq!(toml_display(&v).unwrap(), "false");
        assert!(get_value(&cfg, "repo.default_branch").unwrap().is_none());
        assert!(get_value(&cfg, "nope.nothing").unwrap().is_none());
    }

    #[test]
    fn value_parsing_falls_back_to_string() {
        assert!(parse_toml_value("3").is_integer());
        assert!(parse_toml_value("true").is_bool());
        assert!(parse_toml_value("[1, 2]").is_array());
        assert_eq!(parse_toml_value("auto").as_str(), Some("auto"));
        assert_eq!(parse_toml_value("In Progress").as_str(), Some("In Progress"));
        assert_eq!(parse_toml_value("\"quoted\"").as_str(), Some("quoted"));
    }
}
