//! Queue capacity and repository-backed workflow selection, independent of project routing.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{ClaudeSessionMode, Config};
use crate::domain::{Provider, SessionRuntime, Task};
use crate::priority::{Predicate, policy::predicates, rules::condition_matches};

/// A dedicated pool of execution slots. `default` receives unmatched tasks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    pub max_concurrent: u32,
    pub workflow: Option<String>,
    pub all: Vec<Predicate>,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self { max_concurrent: 1, workflow: None, all: Vec::new() }
    }
}

/// Instructions are relative to the task's checked-out revision, never the host cwd.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowConfig {
    pub instructions: String,
    pub completion: Completion,
    /// Prefix for all source comments from this workflow (e.g. an internal-note marker).
    pub comment_prefix: String,
}

/// Whether finishing the agent's work also drives the source issue lifecycle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Completion {
    #[default]
    Standard,
    LeaveTicketOpen,
}

/// Selection frozen on first scheduling, retained across retries and configuration reloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Execution {
    pub queue: String,
    pub workflow: Option<String>,
    pub policy: Option<WorkflowConfig>,
    #[serde(default)]
    pub outcome: Option<TriageOutcome>,
}

/// The next action recommended by a triage pass, never authorization to implement it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum TriageOutcome {
    NeedsInformation,
    NeedsDecision,
    ReadyForAgent,
    ManualAction,
    NoActionRecommended,
}

impl TriageOutcome {
    pub const ALL: [Self; 5] =
        [Self::NeedsInformation, Self::NeedsDecision, Self::ReadyForAgent, Self::ManualAction, Self::NoActionRecommended];

    /// Qualified Linear label name, matching the documented triage label group.
    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsInformation => "triage/needs-information",
            Self::NeedsDecision => "triage/needs-decision",
            Self::ReadyForAgent => "triage/ready-for-agent",
            Self::ManualAction => "triage/manual-action",
            Self::NoActionRecommended => "triage/no-action-recommended",
        }
    }
}

impl Execution {
    /// True when this workflow must not change source issue states or publish code.
    pub fn leaves_ticket_open(&self) -> bool {
        self.policy.as_ref().is_some_and(|p| p.completion == Completion::LeaveTicketOpen)
    }
}

impl Task {
    /// Persisted lifecycle policy; legacy tasks retain standard completion behavior.
    pub fn leaves_ticket_open(&self) -> bool {
        self.execution.as_ref().is_some_and(Execution::leaves_ticket_open)
    }
}

/// One `[[claude.session_mode_rules]]` row: tasks matching every predicate
/// in `all` run their Claude Code sessions in `mode`. First match wins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionModeRule {
    pub mode: ClaudeSessionMode,
    pub all: Vec<Predicate>,
}

/// Why a task runs in its session mode, most specific first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionModeSource {
    /// `powerqueue task mode <task> <mode>`.
    Command,
    /// The task's key in `[claude.session_mode_overrides]`.
    Override,
    /// The first matching `[[claude.session_mode_rules]]` row (0-based) and
    /// its predicates as written.
    Rule { index: usize, conditions: String },
    /// No override or rule: `claude.session_mode`.
    Default,
}

impl std::fmt::Display for SessionModeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Command => f.write_str("`task mode` override"),
            Self::Override => f.write_str("claude.session_mode_overrides"),
            Self::Rule { index, conditions } => write!(f, "claude.session_mode_rules[{index}] ({conditions})"),
            Self::Default => f.write_str("claude.session_mode default"),
        }
    }
}

/// The session mode chosen for a task and why. Frozen on the task
/// ([`Task::session_mode`]) when it first starts, so crashes, resumes and
/// configuration reloads keep it; only `task mode` replaces it. It applies
/// to Claude Code sessions only: other providers always run in tmux.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModeChoice {
    pub mode: ClaudeSessionMode,
    pub source: SessionModeSource,
}

impl std::fmt::Display for SessionModeChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.mode, self.source)
    }
}

impl Config {
    /// The session mode of `task`: the choice frozen on it at its first
    /// start, else what [`Config::resolve_session_mode`] picks now. A task
    /// that started before modes were recorded (attempts or a start time but
    /// nothing frozen) keeps `claude.session_mode`, the mode it ran in, unless
    /// `task mode` forces one.
    pub fn session_mode_for(&self, task: &Task) -> SessionModeChoice {
        match &task.session_mode {
            Some(frozen) => frozen.clone(),
            None if task.session_mode_override.is_none() && (task.attempts > 0 || task.started_at.is_some()) => {
                SessionModeChoice { mode: self.claude.session_mode, source: SessionModeSource::Default }
            }
            None => self.resolve_session_mode(task),
        }
    }

    /// Pick a session mode ignoring any frozen choice: the task's `task mode`
    /// override, else its key in `claude.session_mode_overrides`
    /// (case-insensitive), else the first `claude.session_mode_rules` row
    /// whose predicates all match, else `claude.session_mode`. A row whose
    /// predicates do not compile never matches (`config validate` reports it).
    pub fn resolve_session_mode(&self, task: &Task) -> SessionModeChoice {
        if let Some(mode) = task.session_mode_override {
            return SessionModeChoice { mode, source: SessionModeSource::Command };
        }
        if let Some(mode) =
            self.claude.session_mode_overrides.iter().find(|(key, _)| key.trim().eq_ignore_ascii_case(&task.key)).map(|(_, m)| *m)
        {
            return SessionModeChoice { mode, source: SessionModeSource::Override };
        }
        for (index, rule) in self.claude.session_mode_rules.iter().enumerate() {
            let Ok(conditions) = predicates(&rule.all) else { continue };
            if conditions.iter().all(|c| condition_matches(c, task)) {
                let conditions = crate::priority::fmt_conditions(&conditions);
                return SessionModeChoice { mode: rule.mode, source: SessionModeSource::Rule { index, conditions } };
            }
        }
        SessionModeChoice { mode: self.claude.session_mode, source: SessionModeSource::Default }
    }

    /// Where `task`'s next session on `provider` runs: headless only for a
    /// Claude Code session whose task mode ([`Config::session_mode_for`]) is
    /// headless; every other provider uses tmux.
    pub fn session_runtime_for(&self, task: &Task, provider: Provider) -> SessionRuntime {
        match (provider, self.session_mode_for(task).mode) {
            (Provider::Claude, ClaudeSessionMode::Headless) => SessionRuntime::Headless,
            _ => SessionRuntime::Tmux,
        }
    }

    /// Whether any task can run headless: the default mode, a rule or an
    /// override says so.
    pub fn headless_possible(&self) -> bool {
        self.claude.session_mode == ClaudeSessionMode::Headless
            || self.claude.session_mode_rules.iter().any(|r| r.mode == ClaudeSessionMode::Headless)
            || self.claude.session_mode_overrides.values().any(|m| *m == ClaudeSessionMode::Headless)
    }

    /// Invalid `claude.session_mode_rules` predicates and blank or duplicate
    /// (case-insensitive) `claude.session_mode_overrides` keys.
    pub fn session_mode_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for (i, rule) in self.claude.session_mode_rules.iter().enumerate() {
            if let Err(e) = predicates(&rule.all) {
                problems.push(format!("claude.session_mode_rules[{i}].all: {e}"));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for key in self.claude.session_mode_overrides.keys() {
            let normal = key.trim().to_ascii_lowercase();
            if normal.is_empty() || normal.contains(char::is_whitespace) {
                problems.push(format!("claude.session_mode_overrides: `{key}` must be a single task key"));
            } else if !seen.insert(normal) {
                problems.push(format!("claude.session_mode_overrides: `{key}` is listed twice (keys are case-insensitive)"));
            }
        }
        problems
    }
}

impl Config {
    /// Resolve a queue/workflow, honoring a prior binding. Ambiguous or missing destinations fail closed.
    pub fn execution_for(&self, task: &Task) -> Result<Execution> {
        if let Some(saved) = &task.execution {
            return Ok(saved.clone());
        }
        // Work already started before queues were configured remains in the default pool.
        let mut selected = "default";
        if task.attempts == 0 && task.started_at.is_none() {
            let mut matched = Vec::new();
            for (name, queue) in &self.queues {
                if name != "default"
                    && predicates(&queue.all).map_err(anyhow::Error::msg)?.iter().all(|p| condition_matches(p, task))
                {
                    matched.push(name.as_str());
                }
            }
            if matched.len() > 1 {
                bail!("task {} matches multiple queues: {}", task.key, matched.join(", "));
            }
            if let Some(name) = matched.first() {
                selected = name;
            }
        }
        let workflow = if task.attempts > 0 || task.started_at.is_some() {
            None // A legacy coding session must not acquire a new workflow when queues are enabled.
        } else {
            self.queues.get(selected).and_then(|q| q.workflow.clone())
        };
        let policy = workflow
            .as_ref()
            .map(|name| self.workflows.get(name).cloned().with_context(|| format!("queue {selected}: unknown workflow {name}")))
            .transpose()?;
        Ok(Execution { queue: selected.into(), workflow, policy, outcome: None })
    }

    /// Validate dedicated capacity, selection predicates and safe workflow paths without filesystem access.
    pub fn execution_problems(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let valid_id = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c));
        if !self.queues.is_empty() && !self.queues.contains_key("default") {
            errors.push("queues.default is required to hold unmatched and pre-existing tasks".into());
        }
        let total: u64 = self.queues.values().map(|q| u64::from(q.max_concurrent)).sum();
        if total > u64::from(self.scheduler.max_concurrent) {
            errors.push(
                "sum of queues.*.max_concurrent must not exceed scheduler.max_concurrent; slots are dedicated, not borrowed"
                    .into(),
            );
        }
        for (name, queue) in &self.queues {
            if !valid_id(name) || queue.max_concurrent == 0 {
                errors.push(format!("queue {name}: use a stable ID and max_concurrent >= 1"));
            }
            if name == "default" && !queue.all.is_empty() || name != "default" && queue.all.is_empty() {
                errors.push(format!("queue {name}: only default may omit all predicates; default must not have predicates"));
            }
            if name != "default"
                && let Err(e) = predicates(&queue.all)
            {
                errors.push(format!("queue {name}: {e}"));
            }
            if let Some(workflow) = &queue.workflow
                && !self.workflows.contains_key(workflow)
            {
                errors.push(format!("queue {name}: unknown workflow {workflow}"));
            }
        }
        for (name, workflow) in &self.workflows {
            if !valid_id(name) || !safe_relative_path(&workflow.instructions) {
                errors.push(format!("workflow {name}: instructions must be a nonempty repository-relative path without .."));
            }
        }
        errors
    }
}

fn safe_relative_path(raw: &str) -> bool {
    !raw.trim().is_empty() && Path::new(raw).components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Read the selected workflow inside a checkout. Missing files and escaping symlinks are errors.
pub fn read_instructions(root: &Path, workflow: &WorkflowConfig) -> Result<String> {
    if !safe_relative_path(&workflow.instructions) {
        bail!("invalid workflow instructions path {}", workflow.instructions);
    }
    let root = root.canonicalize().with_context(|| format!("resolve workflow checkout {}", root.display()))?;
    let path = root.join(&workflow.instructions);
    let resolved = path.canonicalize().with_context(|| format!("resolve workflow instructions {}", path.display()))?;
    if !resolved.starts_with(&root) {
        bail!("workflow instructions {} leave the checkout", path.display());
    }
    let text = std::fs::read_to_string(&resolved).with_context(|| format!("read workflow instructions {}", path.display()))?;
    if text.trim().is_empty() {
        bail!("workflow instructions {} are empty", path.display());
    }
    Ok(text)
}

/// Remaining slots by queue; unknown/ambiguous tasks cannot consume a different queue's capacity.
#[derive(Debug, Clone, Default)]
pub struct QueueSlots {
    remaining: BTreeMap<String, u32>,
    assignments: BTreeMap<crate::domain::TaskId, String>,
    enabled: bool,
}

impl QueueSlots {
    /// Count live sessions (including legacy tasks) against their pool; no I/O.
    pub fn new(cfg: &Config, candidates: &[Task], live: &[Task]) -> Self {
        let mut result = Self {
            remaining: cfg.queues.iter().map(|(n, q)| (n.clone(), q.max_concurrent)).collect(),
            enabled: !cfg.queues.is_empty(),
            ..Self::default()
        };
        for task in candidates.iter().chain(live) {
            if let Ok(execution) = cfg.execution_for(task) {
                result.assignments.insert(task.id, execution.queue);
            }
        }
        for task in live {
            result.started(task);
        }
        result
    }

    /// Whether a candidate has an unoccupied slot in its selected pool.
    pub fn permits(&self, task: &Task) -> bool {
        if !self.enabled {
            return task.execution.as_ref().is_none_or(|e| e.queue == "default");
        }
        self.assignments.get(&task.id).and_then(|q| self.remaining.get(q)).is_some_and(|n| *n > 0)
    }

    /// Reserve a slot for a launch or an already claimed background setup.
    /// Pending setup need not appear among this pass's candidates.
    pub fn started(&mut self, task: &Task) {
        let queue = self
            .assignments
            .get(&task.id)
            .map(String::as_str)
            .or_else(|| task.execution.as_ref().map(|e| e.queue.as_str()))
            .unwrap_or("default");
        if let Some(n) = self.remaining.get_mut(queue) {
            *n = n.saturating_sub(1);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::{TaskSource, TaskState};

    pub(crate) fn config() -> Config {
        Config::from_toml(
            r#"
[repo]
path = "/repo"
[scheduler]
max_concurrent = 3
[queues.default]
max_concurrent = 2
[queues.triage]
max_concurrent = 1
workflow = "triage"
all = [{field = "team", op = "eq", value = "IT"}]
[workflows.triage]
instructions = ".powerqueue/workflows/triage.md"
completion = "leave-ticket-open"
comment_prefix = "[interno]"
"#,
        )
        .unwrap()
    }

    pub(crate) fn it_task() -> Task {
        Task::new(
            "IT-1",
            "Investigate",
            TaskSource::Linear {
                issue_id: "issue-id".into(),
                identifier: "IT-1".into(),
                url: "https://linear.app/IT-1".into(),
                team_key: "IT".into(),
            },
        )
    }

    #[test]
    fn selection_is_independent_of_project_and_pinned_across_reload() {
        let mut cfg = config();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        let mut task = it_task();
        task.execution_project = "application".into();
        let effects = crate::scheduler::transitions::on_execution_selected(&mut task, &cfg).unwrap();
        assert_eq!(effects.len(), 1);
        assert_eq!(task.execution.as_ref().unwrap().queue, "triage");
        let saved = task.execution.clone();
        cfg.queues.clear();
        cfg.workflows.clear();
        assert_eq!(Some(cfg.execution_for(&task).unwrap()), saved);
        assert!(task.leaves_ticket_open());
        assert_eq!(task.execution_project, "application");
    }

    #[test]
    fn ambiguous_queues_and_invalid_capacity_or_paths_fail_closed() {
        let mut cfg = config();
        cfg.queues.insert("other".into(), cfg.queues["triage"].clone());
        assert!(cfg.execution_for(&it_task()).is_err());
        assert!(cfg.execution_problems().iter().any(|e| e.contains("sum")));
        cfg.workflows.get_mut("triage").unwrap().instructions = "../coding.md".into();
        assert!(cfg.execution_problems().iter().any(|e| e.contains("repository-relative")));
        cfg.queues.remove("default");
        assert!(cfg.execution_problems().iter().any(|e| e.contains("queues.default")));
    }

    #[test]
    fn analysis_can_inspect_parent_and_blocked_tickets() {
        let cfg = config();
        let mut task = it_task();
        task.children.push(crate::domain::LinkedIssue {
            key: "AVS-1".into(),
            title: "Fix".into(),
            state_type: "started".into(),
            pr_merged: false,
        });
        assert!(task.is_waiting());
        crate::scheduler::transitions::on_execution_selected(&mut task, &cfg).unwrap();
        assert!(!task.is_waiting());
        assert_eq!(task.state, TaskState::Queued);
    }

    #[test]
    fn workflow_file_never_falls_back_to_coding_prompt() {
        let root = tempfile::tempdir().unwrap();
        let mut cfg = config();
        cfg.repo.path = root.path().display().to_string();
        cfg.prompt.template = Some("coding.md".into());
        cfg.prompt.instructions = Some("IMPLEMENT NOW".into());
        let mut task = it_task();
        task.description = "Literal {{unknown}} in ticket".into();
        let ctx = crate::session::PromptContext {
            provider: crate::domain::Provider::Claude,
            model: None,
            attempt: 1,
            previous_error: None,
            powerqueue: None,
        };
        assert!(crate::session::render_execution_prompt(&task, &cfg, &ctx, root.path()).is_err());
        std::fs::create_dir_all(root.path().join(".powerqueue/workflows")).unwrap();
        std::fs::write(root.path().join(".powerqueue/workflows/triage.md"), "Investigate {{key}} and draft questions.").unwrap();
        let prompt = crate::session::render_execution_prompt(&task, &cfg, &ctx, root.path()).unwrap();
        assert!(prompt.text.contains("Investigate IT-1"));
        assert!(prompt.text.contains("Literal {{unknown}}"));
        assert!(prompt.text.contains("--outcome"));
        assert!(!prompt.text.contains("IMPLEMENT NOW"));
        assert!(!prompt.text.contains("Commit as you go"));
    }

    #[test]
    fn removing_all_queues_holds_previously_bound_triage() {
        let cfg = config();
        let mut task = it_task();
        task.execution = Some(cfg.execution_for(&task).unwrap());
        let slots = QueueSlots::new(&Config::default(), &[task.clone()], &[]);
        assert!(!slots.permits(&task));
        assert!(task.leaves_ticket_open());
    }

    #[test]
    fn legacy_started_tasks_do_not_adopt_new_default_workflow() {
        let mut cfg = config();
        cfg.queues.get_mut("default").unwrap().workflow = Some("triage".into());
        let mut task = it_task();
        task.attempts = 1;
        let execution = cfg.execution_for(&task).unwrap();
        assert_eq!(execution.queue, "default");
        assert!(execution.policy.is_none());
    }

    /// The issue's typical rules: chores, docs and dependency bumps and small
    /// estimates headless; spikes and design work interactive (listed first:
    /// first match wins).
    pub(crate) fn mode_config() -> Config {
        Config::from_toml(
            r#"
[repo]
path = "/repo"
[claude]
session_mode = "interactive"
[[claude.session_mode_rules]]
mode = "interactive"
all = [{ field = "label", op = "matches", value = "^(spike|design|needs-decision)$" }]
[[claude.session_mode_rules]]
mode = "headless"
all = [{ field = "label", op = "matches", value = "^(chore|docs|deps)$" }]
[[claude.session_mode_rules]]
mode = "headless"
all = [{ field = "estimate", op = "lt", value = 3 }]
[claude.session_mode_overrides]
"ENG-9" = "interactive"
"#,
        )
        .unwrap()
    }

    fn manual(key: &str, labels: &[&str], estimate: Option<f64>) -> Task {
        let mut t = Task::new(key, "t", crate::domain::TaskSource::Manual);
        t.labels = labels.iter().map(|l| (*l).to_string()).collect();
        t.estimate = estimate;
        t
    }

    #[test]
    fn session_mode_rules_match_labels_and_estimates_first_match_wins() {
        use crate::config::ClaudeSessionMode::{Headless, Interactive};
        let cfg = mode_config();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        let mode = |t: &Task| cfg.resolve_session_mode(t);
        assert_eq!(mode(&manual("ENG-1", &["chore"], None)).mode, Headless);
        assert!(matches!(mode(&manual("ENG-1", &["docs"], None)).source, SessionModeSource::Rule { index: 1, .. }));
        assert_eq!(mode(&manual("ENG-1", &["bug"], Some(2.0))).mode, Headless);
        assert!(matches!(mode(&manual("ENG-1", &["bug"], Some(2.0))).source, SessionModeSource::Rule { index: 2, .. }));
        // estimate 3 is not < 3; no estimate never matches a numeric predicate.
        assert_eq!(
            mode(&manual("ENG-1", &["bug"], Some(3.0))),
            SessionModeChoice { mode: Interactive, source: SessionModeSource::Default }
        );
        assert_eq!(mode(&manual("ENG-1", &[], None)).source, SessionModeSource::Default);
        // A spike that is also a small chore: the first matching row wins.
        let spike = mode(&manual("ENG-1", &["spike", "chore"], Some(1.0)));
        assert_eq!(
            (spike.mode, &spike.source),
            (Interactive, &SessionModeSource::Rule { index: 0, conditions: "label ~ ^(spike|design|needs-decision)$".into() })
        );
        assert_eq!(spike.to_string(), "interactive (claude.session_mode_rules[0] (label ~ ^(spike|design|needs-decision)$))");
    }

    #[test]
    fn session_mode_precedence_is_task_mode_then_override_then_rule_then_default() {
        use crate::config::ClaudeSessionMode::{Headless, Interactive};
        let mut cfg = mode_config();
        // Config override (case-insensitive key) beats a matching headless rule.
        let mut task = manual("eng-9", &["chore"], Some(1.0));
        assert_eq!(cfg.resolve_session_mode(&task), SessionModeChoice { mode: Interactive, source: SessionModeSource::Override });
        // `task mode` beats the override.
        task.session_mode_override = Some(Headless);
        assert_eq!(cfg.resolve_session_mode(&task), SessionModeChoice { mode: Headless, source: SessionModeSource::Command });
        // The default applies only when nothing matches, and follows claude.session_mode.
        cfg.claude.session_mode = Headless;
        let plain = manual("ENG-2", &["bug"], None);
        assert_eq!(cfg.resolve_session_mode(&plain), SessionModeChoice { mode: Headless, source: SessionModeSource::Default });
    }

    #[test]
    fn frozen_session_mode_survives_reloads_and_only_claude_runs_headless() {
        use crate::config::ClaudeSessionMode::{Headless, Interactive};
        let mut cfg = mode_config();
        let mut task = manual("ENG-1", &["chore"], None);
        task.session_mode = Some(cfg.session_mode_for(&task));
        assert_eq!(cfg.session_runtime_for(&task, Provider::Claude), SessionRuntime::Headless);
        assert_eq!(cfg.session_runtime_for(&task, Provider::Codex), SessionRuntime::Tmux);
        // A reload that drops the rules and flips the default changes nothing.
        cfg.claude.session_mode_rules.clear();
        cfg.claude.session_mode = Interactive;
        assert_eq!(cfg.session_mode_for(&task).mode, Headless);
        assert_eq!(cfg.session_runtime_for(&task, Provider::Claude), SessionRuntime::Headless);
        // Unfrozen tasks follow the configuration as it is now.
        task.session_mode = None;
        assert_eq!(cfg.session_runtime_for(&task, Provider::Claude), SessionRuntime::Tmux);
    }

    #[test]
    fn tasks_started_before_modes_were_recorded_keep_the_default() {
        use crate::config::ClaudeSessionMode::{Headless, Interactive};
        let cfg = mode_config();
        let mut legacy = manual("ENG-1", &["chore"], None);
        legacy.attempts = 1;
        legacy.started_at = Some(chrono::Utc::now());
        assert_eq!(cfg.session_mode_for(&legacy), SessionModeChoice { mode: Interactive, source: SessionModeSource::Default });
        assert_eq!(cfg.resolve_session_mode(&legacy).mode, Headless, "the rules would pick headless");
        // `task mode` still applies, and `auto` chooses by the rules right away.
        crate::scheduler::commands::on_set_session_mode(&mut legacy, Some(Headless), &cfg);
        assert_eq!(legacy.session_mode, Some(SessionModeChoice { mode: Headless, source: SessionModeSource::Command }));
        crate::scheduler::commands::on_set_session_mode(&mut legacy, None, &cfg);
        assert!(matches!(
            legacy.session_mode,
            Some(SessionModeChoice { mode: Headless, source: SessionModeSource::Rule { index: 1, .. } })
        ));
        // A task that never started chooses at its first start.
        let mut fresh = manual("ENG-2", &["chore"], None);
        crate::scheduler::commands::on_set_session_mode(&mut fresh, Some(Interactive), &cfg);
        assert_eq!((fresh.session_mode_override, fresh.session_mode), (Some(Interactive), None));
    }

    #[test]
    fn invalid_session_mode_rules_and_unknown_modes_are_reported() {
        let err = format!(
            "{:#}",
            Config::from_toml(
                "[[claude.session_mode_rules]]\nmode = \"batch\"\nall = [{ field = \"label\", op = \"eq\", value = \"x\" }]\n"
            )
            .unwrap_err()
        );
        assert!(err.contains("batch") || err.contains("mode"), "{err}");
        let err = format!("{:#}", Config::from_toml("[claude.session_mode_overrides]\n\"ENG-1\" = \"tmux\"\n").unwrap_err());
        assert!(err.contains("tmux") || err.contains("session_mode_overrides"), "{err}");
        let cfg = Config::from_toml(
            r#"
[repo]
path = "/repo"
[[claude.session_mode_rules]]
mode = "headless"
all = []
[[claude.session_mode_rules]]
mode = "headless"
all = [{ field = "colour", op = "eq", value = "red" }]
[[claude.session_mode_rules]]
mode = "headless"
all = [{ field = "label", op = "gt", value = 2 }]
[claude.session_mode_overrides]
"ENG-1" = "headless"
"eng-1" = "interactive"
"two words" = "headless"
"#,
        )
        .unwrap();
        let problems = cfg.validate();
        for needle in [
            "session_mode_rules[0].all",
            "session_mode_rules[1].all: unknown field `colour`",
            "session_mode_rules[2].all",
            "listed twice",
            "single task key",
        ] {
            assert!(problems.iter().any(|p| p.contains(needle)), "{needle}: {problems:?}");
        }
        // Rows that do not compile never match.
        assert_eq!(cfg.resolve_session_mode(&manual("ENG-2", &["red"], None)).source, SessionModeSource::Default);
    }

    #[test]
    fn repo_overrides_may_replace_the_session_mode_rules() {
        let mut cfg = mode_config();
        let ov: crate::config::RepoOverrides = toml::from_str(
            "[claude]\nsession_mode_rules = [{ mode = 'headless', all = [{ field = 'label', op = 'eq', value = 'bug' }] }]\n",
        )
        .unwrap();
        cfg.merge_overrides(ov);
        assert_eq!(cfg.claude.session_mode_rules.len(), 1);
        assert_eq!(cfg.resolve_session_mode(&manual("ENG-1", &["bug"], None)).mode, crate::config::ClaudeSessionMode::Headless);
        assert!(cfg.overrides.keys.contains(&"claude.session_mode_rules".to_string()), "{:?}", cfg.overrides.keys);
    }

    #[test]
    fn binding_round_trips_and_survives_an_atomic_start_claim() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let mut task = it_task();
        store.insert_task(&task).unwrap();
        let before = task.clone();
        task.execution = Some(config().execution_for(&task).unwrap());
        task.state = TaskState::Starting;
        task.attempts = 1;
        assert!(store.claim_task_start(&before, &task).unwrap());
        assert_eq!(store.get_task(task.id).unwrap().unwrap().execution, task.execution);
        store.update_task(&task).unwrap();
        assert_eq!(store.get_task(task.id).unwrap().unwrap().execution, task.execution);
    }
}

#[cfg(test)]
mod properties {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn binding_is_idempotent_and_does_not_change_task_state(mut task in crate::strategies::task()) {
            let cfg = super::tests::config();
            let before = task.clone();
            crate::scheduler::transitions::on_execution_selected(&mut task, &cfg).unwrap();
            let bound = task.clone();
            let events = crate::scheduler::transitions::on_execution_selected(&mut task, &Config::default()).unwrap();
            prop_assert!(events.is_empty());
            prop_assert_eq!(&task, &bound);
            task.execution = before.execution.clone();
            prop_assert_eq!(task, before);
        }

        /// `task mode` > `claude.session_mode_overrides` > the first matching
        /// rule > `claude.session_mode`; a frozen choice beats them all.
        #[test]
        fn session_mode_precedence(
            mut task in crate::strategies::task(),
            command in prop::option::of(crate::strategies::session_mode()),
            keyed in prop::option::of(crate::strategies::session_mode()),
            ruled in prop::option::of(crate::strategies::session_mode()),
            default in crate::strategies::session_mode(),
        ) {
            task.session_mode_override = command;
            task.labels.push("pq-mode".into());
            let mut cfg = Config::default();
            cfg.claude.session_mode = default;
            cfg.claude.session_mode_overrides = keyed.map(|m| (task.key.to_ascii_lowercase(), m)).into_iter().collect();
            cfg.claude.session_mode_rules = ruled
                .map(|mode| SessionModeRule {
                    mode,
                    all: vec![Predicate {
                        field: "label".into(),
                        op: crate::priority::policy::Operator::Eq,
                        value: crate::priority::policy::Value::Text("pq-mode".into()),
                    }],
                })
                .into_iter()
                .collect();
            let expected = command
                .map(|m| (m, SessionModeSource::Command))
                .or_else(|| keyed.map(|m| (m, SessionModeSource::Override)))
                .or_else(|| ruled.map(|m| (m, SessionModeSource::Rule { index: 0, conditions: "label: pq-mode".into() })))
                .unwrap_or((default, SessionModeSource::Default));
            let resolved = cfg.resolve_session_mode(&task);
            prop_assert_eq!((resolved.mode, resolved.source), expected);
            // A frozen choice beats them all; a task that started before modes
            // were recorded keeps the default it ran in unless `task mode` says otherwise.
            if let Some(frozen) = &task.session_mode {
                prop_assert_eq!(&cfg.session_mode_for(&task), frozen);
            } else if command.is_none() && (task.attempts > 0 || task.started_at.is_some()) {
                prop_assert_eq!(cfg.session_mode_for(&task), SessionModeChoice { mode: default, source: SessionModeSource::Default });
            } else {
                prop_assert_eq!(cfg.session_mode_for(&task), cfg.resolve_session_mode(&task));
            }
        }

        #[test]
        #[expect(clippy::as_conversions, reason = "Fixture counts and timestamps are bounded to small values by the test inputs.")]
        fn queues_never_borrow_capacity((coding_cap, triage_cap, coding_live, triage_live) in crate::strategies::queue_capacity_case()) {
            let mut cfg = super::tests::config();
            cfg.queues.get_mut("default").unwrap().max_concurrent = coding_cap;
            cfg.queues.get_mut("triage").unwrap().max_concurrent = triage_cap;
            cfg.scheduler.max_concurrent = coding_cap + triage_cap;
            let coding = Task::new("CODE-1", "code", crate::domain::TaskSource::Manual);
            let triage = super::tests::it_task();
            let live = std::iter::repeat_n(coding.clone(), coding_live as usize).chain(std::iter::repeat_n(triage.clone(), triage_live as usize)).collect::<Vec<_>>();
            let mut slots = QueueSlots::new(&cfg, &[coding.clone(), triage.clone()], &live);
            prop_assert_eq!(slots.permits(&coding), coding_live < coding_cap);
            prop_assert_eq!(slots.permits(&triage), triage_live < triage_cap);
            while slots.permits(&coding) { slots.started(&coding); }
            prop_assert_eq!(slots.permits(&triage), triage_live < triage_cap);
        }
    }
}
