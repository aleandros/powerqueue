//! `powerqueue tune`: plain-language configuration changes drafted by a
//! headless Claude Code session.
//!
//! The user looks at `priority simulate` (or the dashboard), says what they
//! expected instead ("ENG-12 should run before ENG-40", "chores are low and
//! use sonnet", "run three tasks at once"), and powerqueue:
//!
//! 1. creates a *draft directory* under `<state>/tune/<id>/` holding copies
//!    of `PRIORITY.md` and `config.toml` (plus the untouched originals in
//!    `original/`), a `CONTEXT.md` with the current queue, tasks and budget,
//!    and the full prompt it sends;
//! 2. runs `claude -p` in that directory with the request, the grammar of
//!    `PRIORITY.md`, the config reference and permission to run the read-only
//!    `powerqueue priority check/simulate --file` and `config validate
//!    --file` commands against the drafts;
//! 3. validates what came back, shows the diff and the simulated queue with
//!    the drafts, and applies the files atomically on confirmation (asking a
//!    running daemon to reload). `--undo` restores the originals.
//!
//! Nothing in this module talks to the live files until [`apply`] or
//! [`undo`] is called; everything else works on the draft directory, which
//! survives for inspection (`meta.json` records the outcome).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cli::TuneScope;
use crate::config::Config;
use crate::paths::Paths;
use crate::priority::PriorityRules;

/// Name of the rules draft inside a draft directory.
pub const PRIORITY_FILE: &str = "PRIORITY.md";
/// Name of the config draft inside a draft directory.
pub const CONFIG_FILE: &str = "config.toml";
/// Sub-directory holding the untouched copies of the live files.
pub const ORIGINAL_DIR: &str = "original";
/// Metadata file written in every draft directory.
pub const META_FILE: &str = "meta.json";
/// The prompt that was sent to the agent.
pub const PROMPT_FILE: &str = "prompt.md";
/// The agent's raw output (`--output-format json`).
pub const RESULT_FILE: &str = "result.json";
/// The state snapshot embedded in the prompt.
pub const CONTEXT_FILE: &str = "CONTEXT.md";

/// Exit code of `powerqueue tune` when a proposal was produced but not
/// applied (no `--yes` and no terminal to ask on).
pub const EXIT_PROPOSED: i32 = 3;

/// Lifecycle of a draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    /// Created; the agent has not finished yet (or powerqueue died while it ran).
    Running,
    /// The agent produced valid changes that are waiting for `--apply` / confirmation.
    Proposed,
    /// The changes were written to the live files.
    Applied,
    /// The live files were restored from `original/`.
    Undone,
    /// The agent finished without changing anything.
    Unchanged,
    /// The agent's files do not parse or validate; nothing was applied.
    Invalid,
    /// The agent failed, timed out or could not be started.
    Failed,
}

impl DraftStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DraftStatus::Running => "running",
            DraftStatus::Proposed => "proposed",
            DraftStatus::Applied => "applied",
            DraftStatus::Undone => "undone",
            DraftStatus::Unchanged => "unchanged",
            DraftStatus::Invalid => "invalid",
            DraftStatus::Failed => "failed",
        }
    }
}

impl std::fmt::Display for DraftStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the agent reported, as stored in `meta.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentReport {
    pub session_id: Option<String>,
    pub num_turns: Option<u64>,
    pub duration_ms: Option<u64>,
    pub total_cost_usd: Option<f64>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub is_error: bool,
    /// Tool calls Claude Code refused (outside `--allowedTools`); a hint when
    /// the agent reports it could not verify something.
    #[serde(default)]
    pub permission_denials: u64,
}

/// `meta.json`: everything about a draft except the files themselves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DraftMeta {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub instruction: String,
    pub scope: TuneScope,
    pub model: String,
    pub status: DraftStatus,
    /// Live paths the drafts stand for, so `--apply` and `--undo` know where to write.
    pub live_priority: PathBuf,
    pub live_config: PathBuf,
    /// Whether the live `PRIORITY.md` existed when the draft was made (if
    /// not, `original/PRIORITY.md` is absent and `--undo` deletes the file).
    pub had_priority: bool,
    /// The agent's final message.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub agent: AgentReport,
    #[serde(default)]
    pub applied_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub undone_at: Option<DateTime<Utc>>,
    /// Validation problems (when `status == Invalid`) or the failure reason.
    #[serde(default)]
    pub problems: Vec<String>,
}

/// A draft directory and its metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub dir: PathBuf,
    pub meta: DraftMeta,
}

/// Sortable, unique draft id: UTC timestamp (to the millisecond) plus a
/// short random suffix.
pub fn new_draft_id(now: DateTime<Utc>) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{}-{}", now.format("%Y%m%dT%H%M%S%3fZ"), &suffix[..6])
}

impl Draft {
    /// Create `<state>/tune/<id>/` with the editable drafts and the
    /// originals. `priority_text` is `None` when the live rules file does
    /// not exist (the draft then starts from the template). Fails when the
    /// directory cannot be created or a file cannot be written.
    pub fn create(
        paths: &Paths,
        id: &str,
        instruction: &str,
        scope: TuneScope,
        model: &str,
        live_priority: &Path,
        priority_text: Option<&str>,
        live_config: &Path,
        config_text: &str,
    ) -> Result<Draft> {
        let dir = paths.tune_dir().join(id);
        let original = dir.join(ORIGINAL_DIR);
        std::fs::create_dir_all(&original).with_context(|| format!("create {}", original.display()))?;
        let rules = priority_text.unwrap_or_else(|| crate::priority::template());
        write(&dir.join(PRIORITY_FILE), rules)?;
        if let Some(text) = priority_text {
            write(&original.join(PRIORITY_FILE), text)?;
        }
        write(&dir.join(CONFIG_FILE), config_text)?;
        write(&original.join(CONFIG_FILE), config_text)?;
        let meta = DraftMeta {
            id: id.to_string(),
            created_at: Utc::now(),
            instruction: instruction.to_string(),
            scope,
            model: model.to_string(),
            status: DraftStatus::Running,
            live_priority: live_priority.to_path_buf(),
            live_config: live_config.to_path_buf(),
            had_priority: priority_text.is_some(),
            summary: None,
            agent: AgentReport::default(),
            applied_at: None,
            undone_at: None,
            problems: Vec::new(),
        };
        let draft = Draft { dir, meta };
        draft.save_meta()?;
        Ok(draft)
    }

    /// Load a draft from its directory. Fails when `meta.json` is missing or unreadable.
    pub fn load(dir: &Path) -> Result<Draft> {
        let file = dir.join(META_FILE);
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("{} is not a tune draft ({} missing)", dir.display(), file.display()))?;
        let meta: DraftMeta = serde_json::from_str(&text).with_context(|| format!("parse {}", file.display()))?;
        Ok(Draft { dir: dir.to_path_buf(), meta })
    }

    /// Every draft under `<state>/tune/`, oldest first. Directories without a
    /// readable `meta.json` are skipped. An absent `tune/` directory is empty.
    pub fn list(paths: &Paths) -> Result<Vec<Draft>> {
        let root = paths.tune_dir();
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e).with_context(|| format!("read {}", root.display())),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Ok(d) = Draft::load(&entry.path()) {
                out.push(d);
            }
        }
        out.sort_by(|a, b| a.meta.created_at.cmp(&b.meta.created_at).then_with(|| a.meta.id.cmp(&b.meta.id)));
        Ok(out)
    }

    /// The newest draft with `status`, if any.
    pub fn latest_with(paths: &Paths, status: DraftStatus) -> Result<Option<Draft>> {
        Ok(Draft::list(paths)?.into_iter().rev().find(|d| d.meta.status == status))
    }

    /// Resolve `--apply`'s argument: `latest` is the newest proposed draft,
    /// anything else a draft directory (or a draft id under `<state>/tune/`).
    pub fn resolve(paths: &Paths, which: &str) -> Result<Draft> {
        if which == "latest" {
            return Draft::latest_with(paths, DraftStatus::Proposed)?.ok_or_else(|| {
                let drafts = Draft::list(paths).unwrap_or_default();
                match drafts.last() {
                    Some(d) => anyhow!(
                        "no proposed tune draft to apply (the newest one, {}, is {}); run `powerqueue tune \"...\"` first",
                        d.dir.display(),
                        d.meta.status
                    ),
                    None => anyhow!("no tune drafts yet; run `powerqueue tune \"what you expect\"` first"),
                }
            });
        }
        let as_path = PathBuf::from(which);
        let dir = if as_path.join(META_FILE).exists() { as_path } else { paths.tune_dir().join(which) };
        Draft::load(&dir)
    }

    pub fn save_meta(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(&self.meta).context("serialise meta.json")?;
        write(&self.dir.join(META_FILE), &(text + "\n"))
    }

    pub fn priority_path(&self) -> PathBuf {
        self.dir.join(PRIORITY_FILE)
    }
    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }
    pub fn original_path(&self, name: &str) -> PathBuf {
        self.dir.join(ORIGINAL_DIR).join(name)
    }
    pub fn prompt_path(&self) -> PathBuf {
        self.dir.join(PROMPT_FILE)
    }

    /// Write a file inside the draft directory.
    pub fn write_file(&self, name: &str, text: &str) -> Result<()> {
        write(&self.dir.join(name), text)
    }

    /// Record a status (and optional problems) in `meta.json`.
    pub fn set_status(&mut self, status: DraftStatus, problems: Vec<String>) -> Result<()> {
        self.meta.status = status;
        self.meta.problems = problems;
        self.save_meta()
    }
}

fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

/// Write `text` to `path` atomically (temp file in the same directory, then
/// rename). `private` sets mode 0600 (config.toml holds nothing secret, but
/// `init` writes it that way and we keep the habit).
pub fn atomic_write(path: &Path, text: &str, private: bool) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into()),
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        if private {
            crate::config::write_private(&tmp, text.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
        } else {
            std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("rename {} to {}", tmp.display(), path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

// ------------------------------------------------------------------ changes

/// One file of a draft compared with the live file it stands for.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileChange {
    /// `PRIORITY.md` or `config.toml`.
    pub name: String,
    /// Where the draft goes when applied.
    pub live: PathBuf,
    /// Current live content (`None` when the live file does not exist).
    #[serde(skip)]
    pub before: Option<String>,
    /// Draft content.
    #[serde(skip)]
    pub after: String,
    pub changed: bool,
    /// Unified diff (empty when unchanged).
    pub diff: String,
    /// True when the live file differs from the copy taken when the draft was
    /// made (someone edited it in between).
    pub live_changed_since_draft: bool,
}

/// Compare the draft files in `scope` with the live files they stand for.
/// Fails when a draft file cannot be read.
pub fn changes(draft: &Draft) -> Result<Vec<FileChange>> {
    let mut out = Vec::new();
    let scope = draft.meta.scope;
    let pairs: Vec<(&str, PathBuf, &Path)> = [
        (PRIORITY_FILE, draft.priority_path(), draft.meta.live_priority.as_path()),
        (CONFIG_FILE, draft.config_path(), draft.meta.live_config.as_path()),
    ]
    .into_iter()
    .filter(|(name, _, _)| in_scope(scope, name))
    .collect();
    for (name, draft_path, live) in pairs {
        let after = std::fs::read_to_string(&draft_path).with_context(|| format!("read {}", draft_path.display()))?;
        let before = read_optional(live)?;
        let original = read_optional(&draft.original_path(name))?;
        let changed = before.as_deref() != Some(after.as_str());
        let diff = if changed { unified_diff(name, before.as_deref().unwrap_or(""), &after) } else { String::new() };
        out.push(FileChange {
            name: name.to_string(),
            live: live.to_path_buf(),
            live_changed_since_draft: before != original,
            before,
            after,
            changed,
            diff,
        });
    }
    Ok(out)
}

fn in_scope(scope: TuneScope, name: &str) -> bool {
    match name {
        PRIORITY_FILE => scope.includes_priority(),
        CONFIG_FILE => scope.includes_config(),
        _ => false,
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// A unified diff of `before` → `after` with `name` in the header lines.
pub fn unified_diff(name: &str, before: &str, after: &str) -> String {
    let diff = similar::TextDiff::from_lines(before, after);
    diff.unified_diff().context_radius(3).header(&format!("a/{name}"), &format!("b/{name}")).to_string()
}

// --------------------------------------------------------------- validation

/// Parsed drafts: what [`validate`] hands back when both files are sound.
#[derive(Debug)]
pub struct ValidDrafts {
    pub rules: PriorityRules,
    /// The draft config with the repository overrides applied (for simulation).
    pub config: Config,
}

/// Parse and validate the draft files. Files outside the draft's scope are
/// validated too (the agent must not have touched them, but a broken file
/// must never be applied). `Err` lists every problem found; `Ok` carries the
/// parsed rules and config.
pub fn validate(paths: &Paths, draft: &Draft) -> std::result::Result<ValidDrafts, Vec<String>> {
    let mut problems = Vec::new();
    let rules = match std::fs::read_to_string(draft.priority_path()) {
        Ok(text) => match PriorityRules::parse(&text) {
            Ok(rules) => Some(rules),
            Err(errors) => {
                problems.extend(errors.iter().map(|e| format!("{PRIORITY_FILE} line {}: {}", e.line, e.message)));
                None
            }
        },
        Err(e) => {
            problems.push(format!("cannot read {PRIORITY_FILE}: {e}"));
            None
        }
    };
    let config = match Config::load_draft(paths, &draft.config_path()) {
        Ok(cfg) => {
            let cfg_problems = cfg.validate();
            if cfg_problems.is_empty() {
                Some(cfg)
            } else {
                problems.extend(cfg_problems.into_iter().map(|p| format!("{CONFIG_FILE}: {p}")));
                None
            }
        }
        Err(e) => {
            problems.push(format!("{CONFIG_FILE}: {e:#}"));
            None
        }
    };
    match (rules, config) {
        (Some(rules), Some(config)) if problems.is_empty() => Ok(ValidDrafts { rules, config }),
        _ => Err(problems),
    }
}

// -------------------------------------------------------------- apply / undo

/// Write every changed file of `changes` to its live path (atomically, one
/// file at a time) and mark the draft applied. Returns the paths written.
/// A failure midway leaves the files already written in place; `--undo`
/// restores all of them.
pub fn apply(draft: &mut Draft, changes: &[FileChange]) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for change in changes.iter().filter(|c| c.changed) {
        atomic_write(&change.live, &change.after, change.name == CONFIG_FILE)?;
        written.push(change.live.clone());
    }
    draft.meta.status = DraftStatus::Applied;
    draft.meta.applied_at = Some(Utc::now());
    draft.save_meta()?;
    Ok(written)
}

/// Restore the live files an applied draft replaced from its `original/`
/// copies. A file that did not exist before the apply is removed. Returns
/// the paths restored or removed. Fails when the draft was not applied.
pub fn undo(draft: &mut Draft) -> Result<Vec<PathBuf>> {
    if draft.meta.status != DraftStatus::Applied {
        bail!("draft {} is {}, not applied; nothing to undo", draft.dir.display(), draft.meta.status);
    }
    let mut touched = Vec::new();
    let scope = draft.meta.scope;
    for (name, live) in [(PRIORITY_FILE, draft.meta.live_priority.clone()), (CONFIG_FILE, draft.meta.live_config.clone())] {
        if !in_scope(scope, name) {
            continue;
        }
        match read_optional(&draft.original_path(name))? {
            Some(text) => {
                if read_optional(&live)?.as_deref() == Some(text.as_str()) {
                    continue; // already what it was
                }
                atomic_write(&live, &text, name == CONFIG_FILE)?;
                touched.push(live);
            }
            None => {
                if live.exists() {
                    std::fs::remove_file(&live).with_context(|| format!("remove {}", live.display()))?;
                    touched.push(live);
                }
            }
        }
    }
    draft.meta.status = DraftStatus::Undone;
    draft.meta.undone_at = Some(Utc::now());
    draft.save_meta()?;
    Ok(touched)
}

/// Remove finished drafts (anything but `running`/`proposed`) beyond the
/// newest `keep` drafts. Returns how many directories were removed.
pub fn prune(paths: &Paths, keep: usize) -> Result<usize> {
    let drafts = Draft::list(paths)?;
    let mut removed = 0;
    let excess = drafts.len().saturating_sub(keep);
    for draft in drafts.iter().take(excess) {
        if matches!(draft.meta.status, DraftStatus::Running | DraftStatus::Proposed) {
            continue;
        }
        std::fs::remove_dir_all(&draft.dir).with_context(|| format!("remove {}", draft.dir.display()))?;
        removed += 1;
    }
    Ok(removed)
}

// ------------------------------------------------------------------- prompt

/// The grammar reference shipped with the binary (`docs/priority.md`).
pub fn priority_reference() -> &'static str {
    include_str!("../docs/priority.md")
}

/// The `## Configuration` chapter of the README (every `[section]` with its
/// keys, defaults and meaning), as shipped with the binary.
pub fn config_reference() -> &'static str {
    static README: &str = include_str!("../README.md");
    section(README, "\n## Configuration", "\n## ").unwrap_or(README)
}

/// The text from the line starting with `start` up to (not including) the
/// next line starting with `next_heading`.
fn section<'a>(text: &'a str, start: &str, next_heading: &str) -> Option<&'a str> {
    let from = text.find(start)?;
    let body = &text[from..];
    let after_heading = body.find('\n').map(|i| i + 1).unwrap_or(body.len());
    let end = body[after_heading..].find(next_heading).map(|i| after_heading + i).unwrap_or(body.len());
    Some(body[..end].trim())
}

/// Build the prompt for the tuning session.
pub fn build_prompt(instruction: &str, scope: TuneScope, context_md: &str, repo_overrides: Option<&str>) -> String {
    let mut p = String::new();
    p.push_str("# powerqueue tuning session\n\n");
    p.push_str(
        "You are editing the configuration of **powerqueue**, a daemon that turns Linear tickets and manual \
         tasks into coding-agent sessions (one git worktree and one tmux window per task) and paces model usage \
         against a subscription budget. The user looked at the current queue and wants it to behave differently. \
         Change the draft files in the current directory so that it does, verify with the simulation, and report.\n\n",
    );
    p.push_str("## The request\n\n");
    p.push_str(instruction.trim());
    p.push_str("\n\n## Files you may edit (current directory)\n\n");
    if scope.includes_priority() {
        p.push_str(
            "- `PRIORITY.md` — the priority rules: criticality, score, skips and preferred models per task (grammar below).\n",
        );
    }
    if scope.includes_config() {
        p.push_str("- `config.toml` — the daemon configuration: concurrency, budgets and pacing, providers, cleanup (reference below).\n");
    }
    match scope {
        TuneScope::Priority => {
            p.push_str("`config.toml` is present for reference only; the user asked for rule changes, so leave it untouched.\n")
        }
        TuneScope::Config => {
            p.push_str("`PRIORITY.md` is present for reference only; the user asked for config changes, so leave it untouched.\n")
        }
        TuneScope::All => {}
    }
    p.push_str(
        "\nBoth files are **drafts**: copies of the live files. Nothing takes effect until the user reviews the diff \
         and applies it. Do not edit anything outside this directory, do not touch `original/`, `CONTEXT.md`, \
         `prompt.md` or `meta.json`, and do not run commands that change the live installation \
         (`powerqueue config set`, `powerqueue priority edit`, `powerqueue add`, `powerqueue task ...`, \
         `powerqueue budget set-*`, `powerqueue reset`, git, package managers).\n\n",
    );
    p.push_str("## How to verify\n\n");
    p.push_str("- `powerqueue priority check --file PRIORITY.md` — parse errors and warnings with line numbers.\n");
    p.push_str(
        "- `powerqueue priority simulate --file PRIORITY.md --config config.toml --reasons` — the resulting queue \
         order, the model each task would get and every rule that fired (add `--no-budget` to rank without the \
         budget policy, `-a` to include finished tasks).\n",
    );
    p.push_str("- `powerqueue config validate --file config.toml` — config problems in plain language.\n\n");
    p.push_str(
        "Run the simulation after editing and compare it with the request. Iterate until it matches, or until you \
         are sure the request cannot be met by these files.\n\n",
    );
    p.push_str("## Guidelines\n\n");
    p.push_str(
        "- Make the smallest change that achieves the request. A rule (`## Critical`, `## Scoring`, ...) when the \
         user describes a kind of task (\"bugs\", \"label x\", \"project y\"); an entry in `## Overrides` when they \
         name one ticket.\n\
         - Ordering, criticality, skipping and model preference belong in `PRIORITY.md`; concurrency, budgets, \
         pacing, cleanup and provider settings belong in `config.toml`.\n\
         - Keep comments, structure and formatting; adjust or append rather than rewrite.\n\
         - Never change `repo.path`, `linear.*` identifiers, paths or anything the request does not mention.\n\
         - The queue order is `score` first, then criticality, then age; the budget policy may still downgrade \
         or throttle a model. If the request is about something the rules cannot decide (for example the policy \
         throttles the model), change nothing and explain.\n\
         - If the request is ambiguous, take the most literal reading and say what you assumed.\n\
         - Finish with a short summary (3–8 lines, plain text or Markdown): what changed, in which file, and what \
         the simulation now shows. If you changed nothing, say why and what the user could do instead. This summary \
         is shown to the user next to the diff.\n\n",
    );
    p.push_str("## Current state\n\n");
    p.push_str(context_md.trim_end());
    p.push('\n');
    if let Some(overrides) = repo_overrides {
        p.push_str("\n### Repository overrides (`.powerqueue.toml`, read-only, applied on top of config.toml)\n\n```toml\n");
        p.push_str(overrides.trim_end());
        p.push_str("\n```\n");
    }
    p.push_str("\n---\n\n# Reference: PRIORITY.md grammar\n\n");
    p.push_str(priority_reference().trim_end());
    p.push_str("\n\n---\n\n# Reference: config.toml\n\n");
    p.push_str(config_reference().trim_end());
    p.push('\n');
    p
}

// -------------------------------------------------------------------- agent

/// Everything needed to run the headless session.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRun {
    /// Full command line (`claude -p ...`), without the prompt (fed on stdin).
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// Extra environment for the child (powerqueue's home, PATH with this binary first).
    pub env: Vec<(String, String)>,
    pub prompt: String,
    pub timeout: Duration,
}

/// What the session produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentOutcome {
    /// The agent's final message (`result` of the JSON output, or raw stdout).
    pub result: String,
    pub report: AgentReport,
    pub stdout: String,
    pub stderr: String,
}

/// Tools the session may use without asking: file edits inside the draft
/// directory and the read-only powerqueue commands that work on drafts.
pub const ALLOWED_TOOLS: [&str; 8] = [
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "Bash(powerqueue priority check*)",
    "Bash(powerqueue priority simulate*)",
    "Bash(powerqueue config validate*)",
];

/// The `claude -p` command line for a tuning session. `binary` is
/// `claude.binary`: its program and leading arguments come first (a
/// per-task template cannot be used here; the caller checks).
pub fn claude_argv(binary: &str, model: &str, extra_args: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = crate::session::BinaryTemplate::parse(binary)
        .ok()
        .and_then(|t| t.host_argv().map(|w| w.to_vec()))
        .unwrap_or_else(|| vec![binary.to_string()]);
    argv.extend([
        "-p".to_string(),
        "--model".to_string(),
        model.to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--permission-mode".to_string(),
        "acceptEdits".to_string(),
        "--allowedTools".to_string(),
        ALLOWED_TOOLS.join(","),
        "--no-session-persistence".to_string(),
    ]);
    argv.extend(extra_args.iter().cloned());
    argv
}

/// `POWERQUEUE_HOME` to export when `paths` is the single-root layout
/// (`--home DIR` or the env var), so `powerqueue` commands the agent runs see
/// the same installation. `None` for the XDG layout, which resolves on its own.
pub fn home_env(paths: &Paths) -> Option<PathBuf> {
    let root = paths.config_dir.parent()?;
    (paths == &Paths::rooted(root)).then(|| root.to_path_buf())
}

/// Environment for the child: `POWERQUEUE_HOME` when needed and `PATH` with
/// this binary's directory first, so `powerqueue` inside the session is the
/// one running now.
pub fn agent_env(paths: &Paths, self_bin: &Path) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Some(home) = home_env(paths) {
        env.push(("POWERQUEUE_HOME".to_string(), home.to_string_lossy().to_string()));
    }
    if let Some(dir) = self_bin.parent() {
        let current = std::env::var_os("PATH").map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let sep = if cfg!(windows) { ";" } else { ":" };
        let path =
            if current.is_empty() { dir.to_string_lossy().to_string() } else { format!("{}{sep}{current}", dir.display()) };
        env.push(("PATH".to_string(), path));
    }
    env
}

/// Run the session: feed the prompt on stdin, collect stdout/stderr, kill it
/// after `timeout`. Fails only when the binary cannot be started; a non-zero
/// exit, a timeout or an error result are reported in the outcome.
pub fn run_agent(run: &AgentRun) -> Result<AgentOutcome> {
    let (program, args) = run.argv.split_first().ok_or_else(|| anyhow!("empty agent command line"))?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(&run.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A nested Claude Code refuses to start when it thinks it is inside another session.
        .env_remove("CLAUDECODE");
    #[cfg(unix)]
    {
        // Own process group, so a timeout can take the CLI's children
        // (running tool commands) down with it instead of leaving them
        // holding the output pipes open.
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    for (k, v) in &run.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().with_context(|| format!("launch `{program}` (set claude.binary or install Claude Code)"))?;

    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin pipe for `{program}`"))?;
    let prompt = run.prompt.clone();
    let writer = std::thread::spawn(move || {
        use std::io::Write as _;
        let _ = stdin.write_all(prompt.as_bytes());
        // Dropping closes the pipe, which tells the CLI the prompt is complete.
    });
    let mut stdout_pipe = child.stdout.take().ok_or_else(|| anyhow!("no stdout pipe for `{program}`"))?;
    let mut stderr_pipe = child.stderr.take().ok_or_else(|| anyhow!("no stderr pipe for `{program}`"))?;
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + run.timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().with_context(|| format!("wait for `{program}`"))? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_tree(&mut child);
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = writer.join();
    // After a kill an orphaned grandchild may still hold a pipe; do not wait
    // on it forever.
    let grace = if timed_out { Duration::from_secs(2) } else { Duration::from_secs(60) };
    let stdout = String::from_utf8_lossy(&join_within(out_reader, grace)).to_string();
    let stderr = String::from_utf8_lossy(&join_within(err_reader, grace)).to_string();

    let mut outcome = parse_agent_output(&stdout);
    outcome.report.exit_code = status.and_then(|s| s.code());
    outcome.report.timed_out = timed_out;
    if timed_out || status.is_some_and(|s| !s.success()) {
        outcome.report.is_error = true;
    }
    outcome.stdout = stdout;
    outcome.stderr = stderr;
    Ok(outcome)
}

/// Kill the child and, on Unix, its whole process group.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]).status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The thread's result, or an empty buffer when it is not done within `wait`.
fn join_within(handle: std::thread::JoinHandle<Vec<u8>>, wait: Duration) -> Vec<u8> {
    let deadline = Instant::now() + wait;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return Vec::new();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    handle.join().unwrap_or_default()
}

/// Interpret `claude -p --output-format json` output. The result object may
/// be the whole stdout or one line of it; anything else is treated as the
/// agent's plain-text answer.
pub fn parse_agent_output(stdout: &str) -> AgentOutcome {
    let candidates = std::iter::once(stdout.trim()).chain(stdout.lines().rev().map(str::trim));
    for text in candidates {
        if text.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else { continue };
        let Some(obj) = value.as_object() else { continue };
        if obj.get("type").and_then(|t| t.as_str()) != Some("result") && !obj.contains_key("result") {
            continue;
        }
        let result = match obj.get("result") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) if !other.is_null() => other.to_string(),
            _ => obj.get("error").and_then(|e| e.as_str()).unwrap_or("").to_string(),
        };
        let report = AgentReport {
            session_id: obj.get("session_id").and_then(|v| v.as_str()).map(str::to_string),
            num_turns: obj.get("num_turns").and_then(|v| v.as_u64()),
            duration_ms: obj.get("duration_ms").and_then(|v| v.as_u64()),
            total_cost_usd: obj.get("total_cost_usd").and_then(|v| v.as_f64()),
            exit_code: None,
            timed_out: false,
            is_error: obj.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false)
                || obj.get("subtype").and_then(|s| s.as_str()).is_some_and(|s| s.starts_with("error")),
            permission_denials: obj.get("permission_denials").and_then(|v| v.as_array()).map(|a| a.len() as u64).unwrap_or(0),
        };
        return AgentOutcome { result, report, stdout: String::new(), stderr: String::new() };
    }
    AgentOutcome {
        result: stdout.trim().to_string(),
        report: AgentReport::default(),
        stdout: String::new(),
        stderr: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        (dir, paths)
    }

    fn make_draft(paths: &Paths, id: &str, rules: Option<&str>) -> Draft {
        let live_priority = paths.priority_file();
        let live_config = paths.config_file();
        std::fs::write(&live_config, "[repo]\npath = \"/tmp/repo\"\n").unwrap();
        if let Some(r) = rules {
            std::fs::write(&live_priority, r).unwrap();
        }
        Draft::create(
            paths,
            id,
            "make INC-1 critical",
            TuneScope::All,
            "sonnet",
            &live_priority,
            rules,
            &live_config,
            "[repo]\npath = \"/tmp/repo\"\n",
        )
        .unwrap()
    }

    #[test]
    fn draft_ids_sort_chronologically() {
        let a = new_draft_id("2026-10-02T10:00:00Z".parse().unwrap());
        let b = new_draft_id("2026-10-02T10:00:01Z".parse().unwrap());
        assert!(a < b, "{a} < {b}");
        assert!(a.starts_with("20261002T100000000Z-"), "{a}");
        assert_eq!(a.len(), "20261002T100000000Z-".len() + 6);
    }

    #[test]
    fn create_load_list_and_resolve() {
        let (_tmp, paths) = paths();
        let d1 = make_draft(&paths, "20260101T000000Z-aaaaaa", Some("## Low\n- label: chore\n"));
        assert_eq!(d1.meta.status, DraftStatus::Running);
        assert!(d1.meta.had_priority);
        assert_eq!(std::fs::read_to_string(d1.original_path(PRIORITY_FILE)).unwrap(), "## Low\n- label: chore\n");
        let mut d2 = make_draft(&paths, "20260101T000001Z-bbbbbb", None);
        assert!(!d2.meta.had_priority);
        assert!(!d2.original_path(PRIORITY_FILE).exists());
        assert!(std::fs::read_to_string(d2.priority_path()).unwrap().contains("## Critical"));

        let list = Draft::list(&paths).unwrap();
        assert_eq!(list.iter().map(|d| d.meta.id.as_str()).collect::<Vec<_>>(), [d1.meta.id.as_str(), d2.meta.id.as_str()]);

        assert!(Draft::resolve(&paths, "latest").is_err(), "nothing proposed yet");
        d2.set_status(DraftStatus::Proposed, vec![]).unwrap();
        assert_eq!(Draft::resolve(&paths, "latest").unwrap().meta.id, d2.meta.id);
        assert_eq!(Draft::resolve(&paths, &d1.meta.id).unwrap().meta.id, d1.meta.id);
        assert_eq!(Draft::resolve(&paths, d1.dir.to_str().unwrap()).unwrap().meta.id, d1.meta.id);
        assert!(Draft::resolve(&paths, "nope").is_err());
        assert_eq!(Draft::latest_with(&paths, DraftStatus::Applied).unwrap(), None);

        // Stray directories without meta.json are ignored.
        std::fs::create_dir_all(paths.tune_dir().join("junk")).unwrap();
        assert_eq!(Draft::list(&paths).unwrap().len(), 2);
        let empty = Paths::rooted(&_tmp.path().join("other"));
        assert!(Draft::list(&empty).unwrap().is_empty());
    }

    #[test]
    fn changes_validate_apply_and_undo() {
        let (_tmp, paths) = paths();
        let mut draft = make_draft(&paths, "20260101T000000Z-cccccc", None);
        // Nothing changed yet.
        let ch = changes(&draft).unwrap();
        assert_eq!(ch.len(), 2);
        let prio = ch.iter().find(|c| c.name == PRIORITY_FILE).unwrap();
        assert!(prio.changed, "a missing live PRIORITY.md vs the template counts as a change");
        assert!(prio.before.is_none());
        assert!(!ch.iter().find(|c| c.name == CONFIG_FILE).unwrap().changed);

        // The agent edits both drafts.
        std::fs::write(draft.priority_path(), "## Overrides\n- INC-1: critical\n").unwrap();
        std::fs::write(draft.config_path(), "[repo]\npath = \"/tmp/repo\"\n\n[scheduler]\nmax_concurrent = 3\n").unwrap();
        let valid = validate(&paths, &draft).unwrap();
        assert_eq!(valid.config.scheduler.max_concurrent, 3);
        assert_eq!(valid.rules.overrides.len(), 1);
        let ch = changes(&draft).unwrap();
        assert!(ch.iter().all(|c| c.changed));
        let cfg = ch.iter().find(|c| c.name == CONFIG_FILE).unwrap();
        assert!(cfg.diff.contains("+max_concurrent = 3"), "{}", cfg.diff);
        assert!(cfg.diff.starts_with("--- a/config.toml\n+++ b/config.toml\n"));
        assert!(!cfg.live_changed_since_draft);

        let written = apply(&mut draft, &ch).unwrap();
        assert_eq!(written, vec![paths.priority_file(), paths.config_file()]);
        assert_eq!(draft.meta.status, DraftStatus::Applied);
        assert!(std::fs::read_to_string(paths.config_file()).unwrap().contains("max_concurrent = 3"));
        assert!(paths.priority_file().exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(paths.config_file()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(Draft::load(&draft.dir).unwrap().meta.status, DraftStatus::Applied);

        // Someone edits the live file afterwards: the draft notices.
        std::fs::write(paths.config_file(), "[repo]\npath = \"/tmp/other\"\n").unwrap();
        let ch = changes(&draft).unwrap();
        assert!(ch.iter().find(|c| c.name == CONFIG_FILE).unwrap().live_changed_since_draft);

        let touched = undo(&mut draft).unwrap();
        assert_eq!(touched.len(), 2);
        assert_eq!(draft.meta.status, DraftStatus::Undone);
        assert!(!paths.priority_file().exists(), "PRIORITY.md did not exist before, so undo removes it");
        assert_eq!(std::fs::read_to_string(paths.config_file()).unwrap(), "[repo]\npath = \"/tmp/repo\"\n");
        assert!(undo(&mut draft).is_err(), "cannot undo twice");
    }

    #[test]
    fn validate_reports_every_problem() {
        let (_tmp, paths) = paths();
        let draft = make_draft(&paths, "20260101T000000Z-dddddd", None);
        std::fs::write(draft.priority_path(), "## Critical\n- bogus: x\n").unwrap();
        std::fs::write(draft.config_path(), "[repo]\npath = \"/tmp/repo\"\n[scheduler]\nmax_concurrent = 0\n").unwrap();
        let problems = validate(&paths, &draft).err().unwrap();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].starts_with("PRIORITY.md line 2:"), "{problems:?}");
        assert!(problems[1].contains("scheduler.max_concurrent"), "{problems:?}");
        std::fs::write(draft.config_path(), "not toml at all [[[").unwrap();
        let problems = validate(&paths, &draft).err().unwrap();
        assert!(problems.iter().any(|p| p.starts_with("config.toml:")), "{problems:?}");
    }

    #[test]
    fn scope_limits_changes_and_undo() {
        let (_tmp, paths) = paths();
        std::fs::write(paths.config_file(), "[repo]\npath = \"/tmp/repo\"\n").unwrap();
        let mut draft = Draft::create(
            &paths,
            "20260101T000000Z-eeeeee",
            "x",
            TuneScope::Priority,
            "sonnet",
            &paths.priority_file(),
            None,
            &paths.config_file(),
            "[repo]\npath = \"/tmp/repo\"\n",
        )
        .unwrap();
        std::fs::write(draft.config_path(), "[repo]\npath = \"/tmp/elsewhere\"\n").unwrap();
        let ch = changes(&draft).unwrap();
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].name, PRIORITY_FILE);
        apply(&mut draft, &ch).unwrap();
        assert!(std::fs::read_to_string(paths.config_file()).unwrap().contains("/tmp/repo"), "config untouched");
        undo(&mut draft).unwrap();
        assert!(!paths.priority_file().exists());
    }

    #[test]
    fn prune_keeps_newest_and_pending() {
        let (_tmp, paths) = paths();
        let mut old = make_draft(&paths, "20260101T000000Z-000000", None);
        old.set_status(DraftStatus::Unchanged, vec![]).unwrap();
        let mut pending = make_draft(&paths, "20260101T000001Z-000001", None);
        pending.set_status(DraftStatus::Proposed, vec![]).unwrap();
        let mut applied = make_draft(&paths, "20260101T000002Z-000002", None);
        applied.set_status(DraftStatus::Applied, vec![]).unwrap();
        let _newest = make_draft(&paths, "20260101T000003Z-000003", None);
        assert_eq!(prune(&paths, 2).unwrap(), 1, "only the old unchanged draft goes; proposed is kept");
        let left: Vec<String> = Draft::list(&paths).unwrap().into_iter().map(|d| d.meta.id).collect();
        assert_eq!(left.len(), 3);
        assert!(left.iter().any(|id| id.ends_with("000001")));
        assert_eq!(prune(&paths, 10).unwrap(), 0);
    }

    #[test]
    fn atomic_write_replaces_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub").join("f.txt");
        atomic_write(&file, "one", false).unwrap();
        atomic_write(&file, "two", true).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("sub")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    }

    #[test]
    fn unified_diff_has_headers_and_markers() {
        let d = unified_diff("PRIORITY.md", "## Low\n- label: chore\n", "## Low\n- label: chore\n- label: docs\n");
        assert!(d.starts_with("--- a/PRIORITY.md\n+++ b/PRIORITY.md\n@@"));
        assert!(d.contains("\n+- label: docs\n"));
        assert_eq!(unified_diff("x", "same\n", "same\n"), "");
    }

    #[test]
    fn references_are_embedded() {
        assert!(priority_reference().starts_with("# PRIORITY.md: the rules grammar"));
        let cfg = config_reference();
        assert!(cfg.starts_with("## Configuration"), "{}", &cfg[..60]);
        assert!(cfg.contains("### `[scheduler]`"));
        assert!(cfg.contains("### `[tune]`"), "the README must document [tune]");
        assert!(!cfg.contains("## Priority rules (PRIORITY.md)"), "the slice stops at the next chapter");
        assert_eq!(section("a\n## B\nbody\n## C\n", "\n## B", "\n## "), Some("## B\nbody"));
        assert_eq!(section("none", "\n## B", "\n## "), None);
    }

    #[test]
    fn prompt_mentions_request_scope_and_tools() {
        let p = build_prompt("INC-1 first", TuneScope::Priority, "## Queue\n(empty)\n", Some("setup = [\"make\"]"));
        assert!(p.contains("## The request\n\nINC-1 first\n"));
        assert!(p.contains("`config.toml` is present for reference only"));
        assert!(p.contains("priority simulate --file PRIORITY.md --config config.toml --reasons"));
        assert!(p.contains("## Queue\n(empty)"));
        assert!(p.contains("setup = [\"make\"]"));
        assert!(p.contains("# Reference: PRIORITY.md grammar"));
        assert!(p.contains("# Reference: config.toml"));
        let p = build_prompt("x", TuneScope::All, "", None);
        assert!(!p.contains("reference only"));
        assert!(!p.contains("Repository overrides"));
    }

    #[test]
    fn argv_env_and_output_parsing() {
        let argv = claude_argv("claude", "opus", &["--verbose".to_string()]);
        assert_eq!(&argv[..2], &["claude", "-p"]);
        assert!(argv.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(argv.windows(2).any(|w| w == ["--output-format", "json"]));
        assert!(argv.windows(2).any(|w| w == ["--permission-mode", "acceptEdits"]));
        let allowed = &argv[argv.iter().position(|a| a == "--allowedTools").unwrap() + 1];
        assert!(allowed.contains("Bash(powerqueue priority simulate*)"));
        assert!(!allowed.contains("config set"));
        assert_eq!(argv.last().unwrap(), "--verbose");

        let dir = tempfile::tempdir().unwrap();
        let rooted = Paths::rooted(dir.path());
        assert_eq!(home_env(&rooted), Some(dir.path().to_path_buf()));
        let xdg = Paths {
            config_dir: "/a/.config/pq".into(),
            data_dir: "/a/.local/share/pq".into(),
            state_dir: "/a/.local/state/pq".into(),
        };
        assert_eq!(home_env(&xdg), None);
        let env = agent_env(&rooted, Path::new("/opt/pq/bin/powerqueue"));
        assert_eq!(env[0].0, "POWERQUEUE_HOME");
        let path = &env.iter().find(|(k, _)| k == "PATH").unwrap().1;
        assert!(path.starts_with("/opt/pq/bin"), "{path}");

        let out = parse_agent_output(
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1234,"num_turns":3,"result":"Changed PRIORITY.md","session_id":"abc","total_cost_usd":0.05}"#,
        );
        assert_eq!(out.result, "Changed PRIORITY.md");
        assert_eq!(out.report.num_turns, Some(3));
        assert_eq!(out.report.session_id.as_deref(), Some("abc"));
        assert!(!out.report.is_error);
        assert_eq!(out.report.permission_denials, 0);
        let out = parse_agent_output(
            r#"{"type":"result","result":"x","permission_denials":[{"tool_name":"Bash"},{"tool_name":"Bash"}]}"#,
        );
        assert_eq!(out.report.permission_denials, 2);
        let out = parse_agent_output(
            "noise\n{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\"result\":\"\"}\n",
        );
        assert!(out.report.is_error);
        let out = parse_agent_output("plain words from a CLI without JSON\n");
        assert_eq!(out.result, "plain words from a CLI without JSON");
        assert_eq!(out.report, AgentReport::default());
    }

    #[test]
    fn run_agent_handles_exit_codes_and_timeouts() {
        let dir = tempfile::tempdir().unwrap();
        let ok = AgentRun {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "cat >/dev/null; echo '{\"type\":\"result\",\"result\":\"hi\",\"num_turns\":1}'".into(),
            ],
            cwd: dir.path().to_path_buf(),
            env: vec![],
            prompt: "prompt".into(),
            timeout: Duration::from_secs(10),
        };
        let out = run_agent(&ok).unwrap();
        assert_eq!(out.result, "hi");
        assert_eq!(out.report.exit_code, Some(0));
        assert!(!out.report.is_error);

        let failing = AgentRun { argv: vec!["sh".into(), "-c".into(), "echo boom >&2; exit 3".into()], ..ok.clone() };
        let out = run_agent(&failing).unwrap();
        assert_eq!(out.report.exit_code, Some(3));
        assert!(out.report.is_error);
        assert!(out.stderr.contains("boom"));

        let hanging = AgentRun {
            // A grandchild keeps the pipes open after the shell dies: the group kill / grace period must cope.
            argv: vec!["sh".into(), "-c".into(), "sleep 30 & sleep 30".into()],
            timeout: Duration::from_millis(300),
            ..ok.clone()
        };
        let started = Instant::now();
        let out = run_agent(&hanging).unwrap();
        assert!(out.report.timed_out);
        assert!(out.report.is_error);
        assert!(started.elapsed() < Duration::from_secs(10));

        let missing = AgentRun { argv: vec!["/definitely/not/a/binary".into()], ..ok };
        assert!(run_agent(&missing).is_err());
    }
}
