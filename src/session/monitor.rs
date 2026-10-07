//! Liveness and resource probes.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate};

use crate::domain::{ResourceSample, Session};
use crate::tmux::Tmux;

/// Result of checking on a session's tmux pane and process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProbe {
    /// The pane still exists in tmux.
    pub pane_exists: bool,
    /// The pane's command has exited (`remain-on-exit` keeps the pane).
    pub pane_dead: bool,
    pub exit_status: Option<i32>,
    /// The shell pid of the pane; children are Claude + tools.
    pub pane_pid: Option<u32>,
    /// The foreground command tmux reports (`node`, `claude`, `bash`...).
    pub current_command: Option<String>,
}

impl SessionProbe {
    pub fn is_alive(&self) -> bool {
        self.pane_exists && !self.pane_dead
    }

    /// Probe for a pane tmux no longer knows about.
    fn missing() -> Self {
        Self { pane_exists: false, pane_dead: false, exit_status: None, pane_pid: None, current_command: None }
    }
}

/// Inspect tmux for the session's pane. A session without a pane id, or
/// whose pane (or whole tmux session) is gone, reports `pane_exists = false`.
pub fn probe_session(tmux: &Tmux, session: &Session) -> Result<SessionProbe> {
    let Some(pane_id) = session.pane_id.as_deref() else {
        return Ok(SessionProbe::missing());
    };
    let probe = match tmux.find_pane(&session.tmux_session, pane_id)? {
        Some(p) => SessionProbe {
            pane_exists: true,
            pane_dead: p.dead,
            exit_status: p.dead_status,
            pane_pid: Some(p.pane_pid),
            current_command: Some(p.current_command),
        },
        None => SessionProbe::missing(),
    };
    tracing::trace!(session = %session.id, pane = pane_id, alive = probe.is_alive(), dead = probe.pane_dead, "probed pane");
    Ok(probe)
}

/// CPU% and RSS summed over the pane's process tree (the pane shell, Claude
/// Code and every tool it spawned). `None` when the session has no pid or the
/// root process no longer exists. CPU is relative to one core, as sysinfo
/// reports it, and is meaningful from the second sample on. Linux threads
/// share their process's RSS and are excluded from all tree totals.
pub fn sample_resources(system: &mut sysinfo::System, session: &Session) -> Option<ResourceSample> {
    let root = Pid::from_u32(session.pid?);
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cpu().with_memory().without_tasks(),
    );
    system.process(root)?;

    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, proc_) in system.processes() {
        // Also exclude thread entries in a System previously populated by
        // another caller. Their memory/CPU is already in the owning process.
        if proc_.thread_kind().is_some() {
            continue;
        }
        if let Some(parent) = proc_.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }

    let mut queue = VecDeque::from([root]);
    let mut seen = HashSet::new();
    let (mut cpu, mut rss, mut count) = (0.0f32, 0u64, 0u32);
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        let Some(p) = system.process(pid) else { continue };
        if p.thread_kind().is_some() {
            continue;
        }
        cpu += p.cpu_usage();
        rss += p.memory();
        count += 1;
        if let Some(kids) = children.get(&pid) {
            queue.extend(kids.iter().copied());
        }
    }
    tracing::trace!(session = %session.id, pid = root.as_u32(), processes = count, cpu, rss, "sampled resources");
    Some(ResourceSample {
        session_id: session.id,
        task_id: session.task_id,
        timestamp: chrono::Utc::now(),
        cpu_percent: cpu,
        rss_bytes: rss,
        process_count: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ModelTier, SessionState, TaskId};

    fn session(pid: Option<u32>, pane_id: Option<&str>) -> Session {
        let now = chrono::Utc::now();
        Session {
            id: uuid::Uuid::new_v4(),
            task_id: TaskId::new(),
            attempt: 1,
            model: ModelTier::sonnet(),
            state: SessionState::Running,
            tmux_session: "powerqueue-test-none".into(),
            tmux_window: "@1".into(),
            pane_id: pane_id.map(String::from),
            pid,
            transcript_path: None,
            exit_code: None,
            started_at: now,
            ended_at: None,
            last_activity_at: now,
            error: None,
            agent_session_id: None,
            waiting_since: None,
            waited_secs: 0,
        }
    }

    #[test]
    fn samples_the_current_process_tree() {
        let mut system = sysinfo::System::new();
        let s = session(Some(std::process::id()), Some("%1"));
        let sample = sample_resources(&mut system, &s).expect("current process exists");
        assert!(sample.process_count >= 1);
        assert!(sample.rss_bytes > 0);
        assert_eq!(sample.session_id, s.id);
        assert_eq!(sample.task_id, s.task_id);
        // A second sample must still find the tree (and not double count).
        let again = sample_resources(&mut system, &s).unwrap();
        assert!(again.process_count >= 1);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn threads_do_not_multiply_process_tree_resources() {
        // Hold worker threads alive throughout both refreshes. A default
        // sysinfo refresh sees them as separate entries with the same RSS.
        let barrier = std::sync::Barrier::new(17);
        let memory = vec![7u8; 8 * 1024 * 1024];
        std::hint::black_box(&memory);
        std::thread::scope(|scope| {
            let mut releases = Vec::new();
            for _ in 0..16 {
                let barrier = &barrier;
                let (release, wait) = std::sync::mpsc::channel::<()>();
                releases.push(release);
                scope.spawn(move || {
                    barrier.wait();
                    let _ = wait.recv();
                });
            }
            barrier.wait();
            let root = Pid::from_u32(std::process::id());
            let mut system = sysinfo::System::new();
            system.refresh_processes(ProcessesToUpdate::All, true);
            let threads = system.process(root).unwrap().tasks().unwrap().len();
            let sample = sample_resources(&mut system, &session(Some(root.as_u32()), None)).unwrap();
            // Calculate the expected total from real descendant processes,
            // including children spawned by other concurrently running tests.
            let mut queue = VecDeque::from([root]);
            let mut seen = HashSet::new();
            let (mut rss, mut cpu) = (0, 0.0f32);
            while let Some(pid) = queue.pop_front() {
                if !seen.insert(pid) {
                    continue;
                }
                let process = system.process(pid).unwrap();
                rss += process.memory();
                cpu += process.cpu_usage();
                queue.extend(
                    system
                        .processes()
                        .iter()
                        .filter(|(_, p)| p.thread_kind().is_none() && p.parent() == Some(pid))
                        .map(|(pid, _)| *pid),
                );
            }
            // Dropping these also releases workers if a probe above panics.
            drop(releases);
            assert!(threads >= 16, "fixture must expose Linux threads");
            assert_eq!(sample.process_count as usize, seen.len());
            assert_eq!(sample.rss_bytes, rss);
            assert!((sample.cpu_percent - cpu).abs() < 0.001);
        });
    }

    #[test]
    fn missing_pid_yields_none() {
        let mut system = sysinfo::System::new();
        assert!(sample_resources(&mut system, &session(None, None)).is_none());
        // Pid 0 is the kernel / not a user process we can see as a child tree root on most systems;
        // use an implausibly large pid instead.
        assert!(sample_resources(&mut system, &session(Some(u32::MAX - 1), None)).is_none());
    }

    #[test]
    fn probe_without_pane_or_server_is_missing() {
        if which::which("tmux").is_err() {
            return;
        }
        let tmux = Tmux::new("tmux", Some(format!("powerqueue-test-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple())));
        let probe = probe_session(&tmux, &session(Some(1), None)).unwrap();
        assert!(!probe.pane_exists && !probe.is_alive());
        // Session name does not exist on this (never started) private socket.
        let probe = probe_session(&tmux, &session(Some(1), Some("%7"))).unwrap();
        assert_eq!(probe, SessionProbe::missing());
    }
}
