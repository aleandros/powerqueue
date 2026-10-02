//! `powerqueue add` — enqueue a manual task.

use std::io::Read;

use anyhow::{Context as _, Result, bail};
use owo_colors::{OwoColorize, Stream, Style};

use crate::cli::{AddArgs, Context};
use crate::domain::{Criticality, EventLevel, ModelTier, Task, TaskSource, TaskState};
use crate::store::Store;

/// What a new manual task should look like; shared by the CLI and the dashboard.
#[derive(Debug, Clone, Default)]
pub struct NewTask {
    pub title: String,
    pub description: String,
    pub key: Option<String>,
    pub criticality: Option<Criticality>,
    pub model_override: Option<ModelTier>,
    pub labels: Vec<String>,
    pub paused: bool,
}

/// Build a [`Task`] from the request without touching the database.
///
/// The key defaults to `manual-<short id>`; the score is the criticality's base
/// score until the daemon re-scores it against `PRIORITY.md`.
pub fn build_task(req: &NewTask) -> Result<Task> {
    let title = req.title.trim();
    if title.is_empty() {
        bail!("task title cannot be empty");
    }
    let mut task = Task::new("", title, TaskSource::Manual);
    task.key = match req.key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => k.to_string(),
        None => format!("manual-{}", task.id.short()),
    };
    task.description = req.description.trim().to_string();
    task.criticality = req.criticality.unwrap_or(Criticality::Normal);
    task.score = task.criticality.base_score();
    task.model_override = req.model_override.clone();
    task.labels = req.labels.iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    if req.paused {
        task.state = TaskState::Paused;
    }
    task.score_reasons = vec![format!("manual task: {} base score {:.0}", task.criticality, task.score)];
    Ok(task)
}

/// `manual-<prefix>` with the shortest id prefix (8, 12, 16, ... hex chars)
/// that is not already taken. UUID v7 short ids share their first 8 chars
/// for about a minute, so two quick `add`s would otherwise collide.
pub fn unique_manual_key(store: &Store, id: crate::domain::TaskId) -> Result<String> {
    let hex = id.0.simple().to_string();
    let mut len = 8;
    while len <= hex.len() {
        let key = format!("manual-{}", &hex[..len]);
        if store.get_task_by_key(&key)?.is_none() {
            return Ok(key);
        }
        len += 4;
    }
    bail!("cannot find a free key for task {id}")
}

/// Insert a manual task and record its `task.created` event.
///
/// Fails with a clear message when the key is already taken.
pub fn create_task(store: &Store, req: &NewTask) -> Result<Task> {
    let mut task = build_task(req)?;
    if store.get_task_by_key(&task.key)?.is_some() {
        if req.key.is_some() {
            bail!("a task with key `{}` already exists", task.key);
        }
        task.key = unique_manual_key(store, task.id)?;
    }
    store.insert_task(&task).with_context(|| format!("insert task {}", task.key))?;
    store.log_event(
        Some(task.id),
        None,
        EventLevel::Info,
        "task.created",
        &format!("manual task created: {}", task.title),
        serde_json::json!({
            "key": task.key,
            "criticality": task.criticality,
            "model_override": task.model_override,
            "labels": task.labels,
            "state": task.state,
        }),
    )?;
    tracing::debug!(task = %task.key, "manual task created");
    Ok(task)
}

/// Handle `powerqueue add`.
pub fn run(ctx: &mut Context, args: AddArgs) -> Result<i32> {
    super::status::ensure_initialised(ctx)?;
    let description = match args.description.as_deref() {
        Some("-") => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).context("read description from stdin")?;
            buf
        }
        Some(d) => d.to_string(),
        None => String::new(),
    };
    let req = NewTask {
        title: args.title,
        description,
        key: args.key,
        criticality: args.criticality,
        model_override: args.model,
        labels: args.label,
        paused: args.paused,
    };
    let store = ctx.store()?.clone();
    let task = create_task(&store, &req)?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&task)?);
        return Ok(0);
    }
    let state = if ctx.color { crate::cli::output::state_colored(task.state) } else { task.state.to_string() };
    println!(
        "{} {} ({}) {}",
        "added".if_supports_color(Stream::Stdout, |t| t.style(Style::new().green().bold())),
        task.key.if_supports_color(Stream::Stdout, |t| t.bold()),
        task.id.short(),
        state
    );
    println!("  {}", task.title);
    if task.state == TaskState::Paused {
        println!(
            "  {}",
            format!("resume with: powerqueue task resume {}", task.key).if_supports_color(Stream::Stdout, |t| t.dimmed())
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_task_defaults() {
        let t = build_task(&NewTask { title: "  Fix login  ".into(), ..Default::default() }).unwrap();
        assert_eq!(t.title, "Fix login");
        assert_eq!(t.key, format!("manual-{}", t.id.short()));
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.criticality, Criticality::Normal);
        assert_eq!(t.source, TaskSource::Manual);
    }

    #[test]
    fn build_task_honours_options() {
        let t = build_task(&NewTask {
            title: "x".into(),
            key: Some("MY-KEY".into()),
            criticality: Some(Criticality::High),
            model_override: Some(ModelTier::opus()),
            labels: vec!["a".into(), " ".into()],
            paused: true,
            description: " body ".into(),
        })
        .unwrap();
        assert_eq!(t.key, "MY-KEY");
        assert_eq!(t.state, TaskState::Paused);
        assert_eq!(t.labels, vec!["a".to_string()]);
        assert_eq!(t.model_override, Some(ModelTier::opus()));
        assert_eq!(t.description, "body");
        assert_eq!(t.score, Criticality::High.base_score());
    }

    #[test]
    fn empty_title_rejected_and_duplicate_key() {
        assert!(build_task(&NewTask::default()).is_err());
        let store = Store::open_in_memory().unwrap();
        let req = NewTask { title: "t".into(), key: Some("K-1".into()), ..Default::default() };
        create_task(&store, &req).unwrap();
        let err = create_task(&store, &req).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // Auto keys never collide even when short ids do.
        let a = create_task(&store, &NewTask { title: "a".into(), ..Default::default() }).unwrap();
        let mut clash = build_task(&NewTask { title: "b".into(), ..Default::default() }).unwrap();
        clash.key = a.key.clone();
        assert_ne!(unique_manual_key(&store, clash.id).unwrap(), a.key);
        let events = store.recent_events(10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "task.created");
    }
}
