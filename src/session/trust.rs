//! Workspace trust for unattended sessions.
//!
//! Interactive Claude Code shows a "do you trust this folder?" dialog the first
//! time it runs in a repository. It keys the answer on the repository's main
//! checkout root and stores it in `~/.claude.json` (or
//! `$CLAUDE_CONFIG_DIR/.claude.json`) as `projects["<root>"].hasTrustDialogAccepted`.
//! The official docs sanction setting that key by hand, which is what we do
//! for the configured repository (and the worktree path, which is harmless)
//! so a task window never sits on the dialog.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Location of Claude Code's global state file: `<claude.env.CLAUDE_CONFIG_DIR>/.claude.json`
/// when the sessions get a config dir of their own (see
/// [`crate::session::transcript::claude_home`]), else the daemon's
/// `$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json`.
pub fn claude_json_path(cfg: &crate::config::Config) -> PathBuf {
    if let Some(dir) = cfg.claude.env.get("CLAUDE_CONFIG_DIR").filter(|d| !d.trim().is_empty()) {
        return crate::paths::expand_tilde(dir).join(".claude.json");
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join(".claude.json");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude.json")
}

/// True if `path` is recorded as trusted in `file`.
pub fn is_trusted(file: &Path, path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else { return false };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
    json["projects"][path.to_string_lossy().as_ref()]["hasTrustDialogAccepted"].as_bool().unwrap_or(false)
}

/// Mark every path in `paths` as trusted in `file`, creating the file if
/// needed. Everything else in the file is preserved byte-for-byte in meaning
/// (it is re-serialised). Returns the paths that were newly marked.
pub fn ensure_trusted(file: &Path, paths: &[&Path]) -> Result<Vec<PathBuf>> {
    let mut json: serde_json::Value = match std::fs::read_to_string(file) {
        Ok(text) if !text.trim().is_empty() => {
            serde_json::from_str(&text).with_context(|| format!("parse {}", file.display()))?
        }
        _ => serde_json::json!({}),
    };
    if !json.is_object() {
        anyhow::bail!("{} is not a JSON object", file.display());
    }
    let projects = json.as_object_mut().expect("checked is_object").entry("projects").or_insert_with(|| serde_json::json!({}));
    if !projects.is_object() {
        *projects = serde_json::json!({});
    }
    let mut changed = Vec::new();
    for path in paths {
        let key = path.to_string_lossy().to_string();
        let entry = projects.as_object_mut().expect("object").entry(key).or_insert_with(|| serde_json::json!({}));
        if !entry.is_object() {
            *entry = serde_json::json!({});
        }
        if entry["hasTrustDialogAccepted"].as_bool() != Some(true) {
            entry["hasTrustDialogAccepted"] = serde_json::Value::Bool(true);
            changed.push(path.to_path_buf());
        }
    }
    if changed.is_empty() {
        return Ok(changed);
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = file.with_extension("json.powerqueue-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&json)?).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, file).with_context(|| format!("replace {}", file.display()))?;
    Ok(changed)
}

/// Paths whose trust matters for a session: the repository root (what Claude
/// keys on for worktrees) and the worktree itself. Both are canonicalised the
/// way Claude Code sees them.
pub fn trust_targets(repo: &Path, worktree: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in [repo, worktree] {
        let physical = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        if !out.contains(&physical) {
            out.push(physical);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_paths_and_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");
        std::fs::write(
            &file,
            r#"{"numStartups": 3, "projects": {"/a": {"allowedTools": ["x"], "hasTrustDialogAccepted": false}}}"#,
        )
        .unwrap();
        let changed = ensure_trusted(&file, &[Path::new("/a"), Path::new("/b")]).unwrap();
        assert_eq!(changed, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(json["numStartups"], 3);
        assert_eq!(json["projects"]["/a"]["allowedTools"][0], "x");
        assert!(is_trusted(&file, Path::new("/a")));
        assert!(is_trusted(&file, Path::new("/b")));
        assert!(!is_trusted(&file, Path::new("/c")));
        // Idempotent.
        assert!(ensure_trusted(&file, &[Path::new("/a")]).unwrap().is_empty());
    }

    #[test]
    fn creates_missing_file_and_rejects_non_object() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested/.claude.json");
        ensure_trusted(&file, &[Path::new("/x")]).unwrap();
        assert!(is_trusted(&file, Path::new("/x")));
        std::fs::write(&file, "[]").unwrap();
        assert!(ensure_trusted(&file, &[Path::new("/x")]).is_err());
    }

    #[test]
    fn trust_targets_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let t = trust_targets(dir.path(), dir.path());
        assert_eq!(t.len(), 1);
    }
}
