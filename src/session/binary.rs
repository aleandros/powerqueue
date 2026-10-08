//! `claude.binary` / `codex.binary` / `gemini.binary` as a command template.
//!
//! The setting used to be a bare executable name. It is now a command line
//! (split like a shell would, so quotes work) whose words may carry per-task
//! placeholders, e.g.
//!
//! ```toml
//! [claude]
//! binary = "docker exec -it --env-file {task_dir}/env -w {worktree} pq-{slug} claude"
//! ```
//!
//! The first word is the program; the rest become leading arguments, and
//! the provider's own flags and the prompt follow. Placeholders are filled
//! after splitting, so values with spaces never need quoting. A template
//! without placeholders can also be run outside a task (`doctor`, the usage
//! probes, `powerqueue tune`); one with placeholders cannot, and those
//! callers say so instead of guessing.

use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Result, anyhow, bail};
use regex::Regex;

use crate::session::agent::LaunchContext;

/// Placeholder names a `binary` template may use (`{name}`).
pub const BINARY_PLACEHOLDERS: [&str; 9] =
    ["key", "slug", "task_id", "session_id", "worktree", "task_dir", "repo", "attempt", "model"];

static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([a-z_]+)\}").expect("valid placeholder regex"));

/// A parsed `binary` setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryTemplate {
    words: Vec<String>,
}

/// Per-task values for the placeholders.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BinaryVars {
    pub key: String,
    pub slug: String,
    pub task_id: String,
    pub session_id: String,
    pub worktree: String,
    pub task_dir: String,
    pub repo: String,
    pub attempt: String,
    pub model: String,
}

impl BinaryVars {
    /// The values of a launch.
    pub fn from_launch(ctx: &LaunchContext<'_>) -> Self {
        Self {
            key: ctx.task.key.clone(),
            slug: ctx.task.slug(),
            task_id: ctx.task.id.to_string(),
            session_id: ctx.session_id.to_string(),
            worktree: ctx.worktree.to_string_lossy().to_string(),
            task_dir: ctx.task_dir.to_string_lossy().to_string(),
            repo: ctx.cfg.repo_path().to_string_lossy().to_string(),
            attempt: ctx.attempt.to_string(),
            model: ctx.model.alias().to_string(),
        }
    }

    fn get(&self, name: &str) -> Option<&str> {
        Some(match name {
            "key" => &self.key,
            "slug" => &self.slug,
            "task_id" => &self.task_id,
            "session_id" => &self.session_id,
            "worktree" => &self.worktree,
            "task_dir" => &self.task_dir,
            "repo" => &self.repo,
            "attempt" => &self.attempt,
            "model" => &self.model,
            _ => return None,
        })
    }
}

impl BinaryTemplate {
    /// Split `template` like a POSIX shell (quotes and backslashes honoured).
    /// Fails on an empty template or unbalanced quotes.
    pub fn parse(template: &str) -> Result<Self> {
        let words = shlex::split(template).ok_or_else(|| anyhow!("unbalanced quotes in command `{}`", template.trim()))?;
        if words.is_empty() || words[0].trim().is_empty() {
            bail!("the command is empty");
        }
        Ok(Self { words })
    }

    /// The program (first word), with placeholders unexpanded.
    pub fn program(&self) -> &str {
        &self.words[0]
    }

    /// Every word, with placeholders unexpanded.
    pub fn words(&self) -> &[String] {
        &self.words
    }

    /// Placeholder names used by the template, in order of first use.
    pub fn placeholders(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for w in &self.words {
            for cap in PLACEHOLDER.captures_iter(w) {
                let name = cap[1].to_string();
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out
    }

    /// Placeholders that are not in [`BINARY_PLACEHOLDERS`].
    pub fn unknown_placeholders(&self) -> Vec<String> {
        self.placeholders().into_iter().filter(|p| !BINARY_PLACEHOLDERS.contains(&p.as_str())).collect()
    }

    /// True when the command depends on a task (it has placeholders).
    pub fn is_per_task(&self) -> bool {
        !self.placeholders().is_empty()
    }

    /// The words with every placeholder replaced. Unknown placeholders are
    /// left verbatim (validation reports them before any launch).
    pub fn render(&self, vars: &BinaryVars) -> Vec<String> {
        self.words
            .iter()
            .map(|w| {
                PLACEHOLDER
                    .replace_all(w, |cap: &regex::Captures<'_>| vars.get(&cap[1]).unwrap_or(&cap[0]).to_string())
                    .into_owned()
            })
            .collect()
    }

    /// The command as it can be run outside a task (`None` when it has
    /// placeholders and so only makes sense for a launch).
    pub fn host_argv(&self) -> Option<&[String]> {
        (!self.is_per_task()).then_some(self.words.as_slice())
    }
}

/// The program word of `binary` (the whole string when it does not parse),
/// for PATH lookups.
pub fn program_of(binary: &str) -> String {
    BinaryTemplate::parse(binary).map(|t| t.program().to_string()).unwrap_or_else(|_| binary.trim().to_string())
}

/// Render `binary` for a launch. Fails when it does not parse.
pub fn launch_argv(binary: &str, ctx: &LaunchContext<'_>) -> Result<Vec<String>> {
    let t = BinaryTemplate::parse(binary)?;
    Ok(t.render(&BinaryVars::from_launch(ctx)))
}

/// A [`std::process::Command`] for running `binary` outside a task: the
/// program plus its leading arguments. Fails when the template does not
/// parse or needs a task (placeholders), so the caller can explain why the
/// check or probe was not run.
pub fn host_command(binary: &str) -> Result<std::process::Command> {
    let t = BinaryTemplate::parse(binary)?;
    let Some(argv) = t.host_argv() else {
        bail!(
            "`{}` is a per-task command (placeholders {}); it cannot run outside a task",
            binary.trim(),
            t.placeholders().iter().map(|p| format!("{{{p}}}")).collect::<Vec<_>>().join(", ")
        );
    };
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    Ok(cmd)
}

/// Where the program of `binary` resolves: on PATH for a bare name, as a
/// file for a path.
pub fn which_program(binary: &str) -> Option<std::path::PathBuf> {
    let program = program_of(binary);
    if Path::new(&program).components().count() > 1 {
        let p = Path::new(&program);
        return p.is_file().then(|| p.to_path_buf());
    }
    which::which(program).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> BinaryVars {
        BinaryVars {
            key: "ENG-7".into(),
            slug: "eng-7".into(),
            task_id: "t-id".into(),
            session_id: "s-id".into(),
            worktree: "/w/eng 7".into(),
            task_dir: "/s/tasks/t-id".into(),
            repo: "/repo".into(),
            attempt: "2".into(),
            model: "opus".into(),
        }
    }

    #[test]
    fn plain_binary_is_one_word() {
        let t = BinaryTemplate::parse("claude").unwrap();
        assert_eq!(t.program(), "claude");
        assert_eq!(t.host_argv().unwrap(), &["claude".to_string()]);
        assert!(!t.is_per_task());
        assert_eq!(t.render(&vars()), vec!["claude".to_string()]);
        assert_eq!(program_of("  /usr/local/bin/claude "), "/usr/local/bin/claude");
    }

    #[test]
    fn docker_template_renders_per_task() {
        let t = BinaryTemplate::parse("docker exec -it --env-file {task_dir}/env -w {worktree} pq-{slug} claude").unwrap();
        assert_eq!(t.program(), "docker");
        assert!(t.is_per_task());
        assert!(t.host_argv().is_none());
        assert_eq!(t.placeholders(), vec!["task_dir", "worktree", "slug"]);
        assert!(t.unknown_placeholders().is_empty());
        let argv = t.render(&vars());
        assert_eq!(
            argv,
            vec!["docker", "exec", "-it", "--env-file", "/s/tasks/t-id/env", "-w", "/w/eng 7", "pq-eng-7", "claude"]
        );
        assert!(host_command(&t.words().join(" ")).unwrap_err().to_string().contains("per-task"));
    }

    #[test]
    fn quotes_and_errors() {
        let t = BinaryTemplate::parse(r#"'/opt/my tools/claude' --flag "a b""#).unwrap();
        assert_eq!(t.words(), &["/opt/my tools/claude", "--flag", "a b"]);
        assert!(BinaryTemplate::parse("").unwrap_err().to_string().contains("empty"));
        assert!(BinaryTemplate::parse("   ").unwrap_err().to_string().contains("empty"));
        assert!(BinaryTemplate::parse("claude 'oops").unwrap_err().to_string().contains("unbalanced"));
        let t = BinaryTemplate::parse("run {nope} {key} {{.X}}").unwrap();
        assert_eq!(t.unknown_placeholders(), vec!["nope"]);
        // Unknown placeholders stay verbatim; braces that are not a placeholder are untouched.
        assert_eq!(t.render(&vars()), vec!["run", "{nope}", "ENG-7", "{{.X}}"]);
        assert_eq!(program_of("claude 'oops"), "claude 'oops");
    }

    #[test]
    fn host_command_runs_leading_args() {
        if which::which("sh").is_err() {
            return;
        }
        let out = host_command("sh -c 'echo hi'").unwrap().output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
        assert!(which_program("sh -c x").is_some());
        assert!(which_program("/definitely/not/here/sh").is_none());
    }
}
