//! Queue capacity and repository-backed workflow selection, independent of project routing.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::domain::Task;
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
