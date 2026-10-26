//! Persistence fault injection at real launch boundaries. Git and tmux are
//! disposable; the agent is a shell fixture and never contacts a provider.

use super::*;
use crate::secrets::FileBackend;
use crate::tmux::LaunchTarget;
use std::os::unix::fs::PermissionsExt;

struct Harness {
    daemon: Daemon,
    writer: rusqlite::Connection,
    tmux: Tmux,
    dir: tempfile::TempDir,
}

impl Harness {
    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn new() -> Option<Self> {
        if which::which("git").is_err() || which::which("tmux").is_err() {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"],
        ] {
            let output = std::process::Command::new("git").args(args).current_dir(&repo).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        }
        let agent = dir.path().join("agent.sh");
        std::fs::write(&agent, "#!/bin/sh\nexec sleep 600\n").unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let paths = Paths::rooted(dir.path());
        paths.ensure().unwrap();
        let db = dir.path().join("state.db");
        let store = Store::open(&db).unwrap();
        let writer = rusqlite::Connection::open(&db).unwrap();
        let mut cfg = Config::default();
        cfg.repo.path = repo.display().to_string();
        cfg.repo.fetch_before_start = false;
        cfg.claude.binary = agent.display().to_string();
        cfg.claude.trust_workspace = false;
        cfg.claude.env.insert("CLAUDE_CONFIG_DIR".into(), dir.path().join("claude").display().to_string());
        cfg.tmux.socket_name = Some(format!("powerqueue-test-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
        cfg.linear.enabled = false;
        cfg.scheduler.restart_backoff_secs = vec![17];
        let tmux = Tmux::new(&cfg.tmux.binary, cfg.tmux.socket_name.clone());
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
        let daemon = Daemon::new(cfg, paths, store, secrets).unwrap();
        Some(Self { daemon, writer, tmux, dir })
    }

    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn task(&self) -> Task {
        let task = Task::new("START-1", "test abandoned start", TaskSource::Manual);
        self.daemon.store.insert_task(&task).unwrap();
        task
    }

    async fn start(&mut self, task: Task) -> Result<Started> {
        let decision = Decision {
            model: Some(ModelTier::sonnet()),
            retry_at: None,
            prediction: crate::budget::Prediction::default_guess(),
            reasons: vec!["test".into()],
        };
        Box::pin(self.daemon.start_task(task, ModelTier::sonnet(), &decision, Utc::now())).await
    }

    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn saved(&self, task: &Task) -> Task {
        self.daemon.store.get_task(task.id).unwrap().unwrap()
    }

    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn restart(&mut self) {
        let paths = self.daemon.paths.clone();
        let cfg = self.daemon.cfg.clone();
        let store = Store::open(&self.dir.path().join("state.db")).unwrap();
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
        self.daemon = Daemon::new(cfg, paths, store, secrets).unwrap();
    }

    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn target(&self, task: &Task) -> LaunchTarget {
        self.daemon.store.kv_get(&LaunchTarget::key(task.id)).unwrap().unwrap()
    }

    #[expect(
        clippy::unwrap_used,
        reason = "Fixture setup and assertions intentionally fail the test immediately on invalid data or I/O."
    )]
    fn stop_panes(&self, task: &Task) {
        let target = self.target(task);
        for pane in self.tmux.list_panes(&target.session_name).unwrap() {
            if pane.window_name != "powerqueue" {
                self.tmux.kill_window(&pane.window_id).unwrap();
            }
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.tmux.command().arg("kill-server").output();
    }
}

/// Invoked by the parent regression in its own process so the launch can be
/// killed mid-setup without killing the test runner.
#[tokio::test]
#[ignore = "subprocess helper for starting_setup_survives_killed_daemon"]
async fn starting_subprocess_worker() {
    let Some(root) = std::env::var_os("PQ_RECOVERY_WORKER_ROOT") else { return };
    let root = PathBuf::from(root);
    let paths = Paths::rooted(&root);
    let cfg = Config::load(&paths).unwrap();
    let store = Store::open(&root.join("state.db")).unwrap();
    let task = store.get_task_by_key("START-1").unwrap().unwrap();
    let secrets = Secrets::with_backend(Box::new(FileBackend::new(paths.secrets_file())));
    let mut daemon = Daemon::new(cfg, paths, store, secrets).unwrap();
    // Hold exactly the same OS lock as Daemon::run, but isolate just launch so
    // unrelated poll/probe phases cannot touch the fixture.
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&daemon.lock_path).unwrap();
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.try_write().unwrap();
    let decision = Decision {
        model: Some(ModelTier::sonnet()),
        retry_at: None,
        prediction: crate::budget::Prediction::default_guess(),
        reasons: vec!["test".into()],
    };
    assert_eq!(Box::pin(daemon.start_task(task, ModelTier::sonnet(), &decision, Utc::now())).await.unwrap(), Started::Preparing);
    // Background setup no longer holds this call. Keep the owning daemon and
    // its exclusive lock alive until the parent kills this process.
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn starting_setup_survives_killed_daemon() {
    let Some(mut h) = Harness::new() else { return };
    let task = h.task();
    let ready = h.dir.path().join("setup.pid");
    h.daemon.cfg.repo.setup = vec![format!("echo $$ > {}; exec sleep 600", crate::tmux::shell_quote(&ready.to_string_lossy()))];
    h.daemon.cfg.save(&h.daemon.paths).unwrap();
    #[expect(
        clippy::items_after_statements,
        reason = "The local helper type is scoped beside the resource or compatibility operation that uses it."
    )]
    struct Processes {
        worker: std::process::Child,
        setup: Option<u32>,
    }
    #[expect(
        clippy::items_after_statements,
        reason = "The local helper type is scoped beside the resource or compatibility operation that uses it."
    )]
    impl Drop for Processes {
        fn drop(&mut self) {
            let _ = self.worker.kill();
            let _ = self.worker.wait();
            if let Some(pid) = self.setup {
                let _ = std::process::Command::new("kill").args(["-KILL", &pid.to_string()]).output();
            }
        }
    }
    let mut processes = Processes {
        worker: std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "scheduler::daemon::start_recovery_tests::starting_subprocess_worker", "--ignored"])
            .env("PQ_RECOVERY_WORKER_ROOT", h.dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
        setup: None,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(&ready).ok().and_then(|s| s.trim().parse::<u32>().ok()) {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "worker did not enter setup");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    processes.setup = Some(pid);
    assert_eq!(h.saved(&task).state, TaskState::Starting);
    // The daemon's exclusive lock prevents a second daemon during slow setup.
    let file = std::fs::OpenOptions::new().write(true).open(&h.daemon.lock_path).unwrap();
    let mut lock = fd_lock::RwLock::new(file);
    assert!(lock.try_write().is_err());
    processes.worker.kill().unwrap();
    processes.worker.wait().unwrap();
    let _guard = lock.try_write().expect("daemon exited, releasing its lock");
    let lease = h.daemon.paths.task_dir(&task.id.to_string()).join("setup.lock");
    assert!(crate::worktree::setup_in_progress(&lease).unwrap(), "setup child retains lease after daemon death");
    h.restart();
    h.daemon.recover_starting(Utc::now()).unwrap();
    assert_eq!(h.saved(&task).state, TaskState::Starting, "surviving setup prevents a second launch");
    assert!(std::process::Command::new("kill").args(["-KILL", &pid.to_string()]).status().unwrap().success());
    processes.setup = None;
    while crate::worktree::setup_in_progress(&lease).unwrap() {
        assert!(std::time::Instant::now() < deadline, "setup did not exit");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    h.daemon.recover_starting(Utc::now()).unwrap();
    assert_eq!(h.saved(&task).state, TaskState::Crashed);
    assert_eq!(h.saved(&task).attempts, 1);
}

#[tokio::test]
async fn starting_claim_and_runtime_target_are_atomic() {
    let Some(mut h) = Harness::new() else { return };
    let task = h.task();
    h.writer.execute_batch("CREATE TRIGGER inject BEFORE INSERT ON kv WHEN NEW.key LIKE 'launch.target.%' BEGIN SELECT RAISE(ABORT, 'target write failed'); END;").unwrap();
    assert!(Box::pin(h.start(task.clone())).await.is_err());
    assert_eq!(h.saved(&task).state, TaskState::Queued);
    assert_eq!(h.saved(&task).attempts, 0);
    assert!(h.daemon.store.kv_get::<LaunchTarget>(&LaunchTarget::key(task.id)).unwrap().is_none());
}

#[tokio::test]
async fn starting_recovers_binding_and_crash_persistence_failures_after_restart() {
    for boundary in ["binding", "crash"] {
        let Some(mut h) = Harness::new() else { return };
        let task = h.task();
        if boundary == "binding" {
            h.writer.execute_batch("CREATE TRIGGER inject BEFORE UPDATE ON tasks WHEN NEW.execution_repo IS NOT OLD.execution_repo BEGIN SELECT RAISE(ABORT, 'binding write failed'); END;").unwrap();
        } else {
            h.daemon.cfg.repo.setup = vec!["exit 1".into()];
            h.writer.execute_batch("CREATE TRIGGER inject BEFORE UPDATE ON tasks WHEN NEW.state='crashed' BEGIN SELECT RAISE(ABORT, 'crash write failed'); END;").unwrap();
        }
        if boundary == "binding" {
            assert!(Box::pin(h.start(task.clone())).await.is_err(), "{boundary}");
        } else {
            assert_eq!(Box::pin(h.start(task.clone())).await.unwrap(), Started::Preparing);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !h.daemon.rt.setups[&task.id].worker.is_finished() {
                assert!(std::time::Instant::now() < deadline, "setup failed to exit");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Box::pin(h.daemon.poll_setups(Utc::now())).await.unwrap();
        }
        let stranded = h.saved(&task);
        assert_eq!(stranded.state, TaskState::Starting, "{boundary}");
        assert_eq!(stranded.attempts, 1);
        assert_eq!((h.daemon.store.list_sessions_for_task(task.id).unwrap()).len(), 0);
        h.writer.execute_batch("DROP TRIGGER inject;").unwrap();
        h.restart();
        h.writer
            .execute_batch(
                "CREATE TRIGGER inject BEFORE UPDATE ON tasks BEGIN SELECT RAISE(ABORT, 'recovery write failed'); END;",
            )
            .unwrap();
        assert!(h.daemon.recover_starting(Utc::now()).is_err());
        assert_eq!(h.saved(&task), stranded);
        h.writer.execute_batch("DROP TRIGGER inject;").unwrap();
        let now = Utc::now();
        h.daemon.recover_starting(now).unwrap();
        let recovered = h.saved(&task);
        assert_eq!(recovered.state, TaskState::Crashed);
        assert_eq!(recovered.attempts, 1);
        assert_eq!(recovered.not_before.unwrap().timestamp(), (now + Duration::seconds(17)).timestamp());
        assert_eq!(recovered.worktree_path, stranded.worktree_path);
        h.daemon.recover_starting(now).unwrap();
        assert_eq!(h.saved(&task), recovered, "recovery is idempotent");
    }
}

#[tokio::test]
async fn starting_session_insert_failure_holds_renamed_live_pane_across_restart_and_config_change() {
    let Some(mut h) = Harness::new() else { return };
    let task = h.task();
    h.writer
        .execute_batch("CREATE TRIGGER inject BEFORE INSERT ON sessions BEGIN SELECT RAISE(ABORT, 'session insert failed'); END;")
        .unwrap();
    assert!(Box::pin(h.start(task.clone())).await.is_err());
    assert_eq!(h.saved(&task).state, TaskState::Starting);
    assert!(h.target(&task).has_live_pane().unwrap());
    h.writer.execute_batch("DROP TRIGGER inject;").unwrap();
    let pane =
        h.tmux.list_panes(&h.daemon.cfg.tmux.session_name).unwrap().into_iter().find(|p| p.window_name == task.slug()).unwrap();
    assert!(h.tmux.command().args(["rename-window", "-t", &pane.window_id, "renamed"]).status().unwrap().success());
    assert!(h.target(&task).has_live_pane().unwrap(), "original command identifies renamed windows");
    h.daemon.cfg.tmux.session_name = "changed".into();
    h.daemon.cfg.tmux.socket_name = Some("unused-recovery-test-socket".into());
    h.restart();
    h.daemon.recover_starting(Utc::now()).unwrap();
    assert_eq!(h.saved(&task).state, TaskState::Starting);
    assert_eq!((h.daemon.store.list_sessions_for_task(task.id).unwrap()).len(), 0);
    h.stop_panes(&task);
    h.daemon.recover_starting(Utc::now()).unwrap();
    assert_eq!(h.saved(&task).state, TaskState::Crashed);
    assert_eq!(h.saved(&task).attempts, 1);
    assert!(h.daemon.store.events_for_task(task.id, 50).unwrap().iter().any(|e| e.kind == "task.start_recovery_deferred"));
}

#[tokio::test]
async fn starting_running_write_failure_keeps_registered_session() {
    let Some(mut h) = Harness::new() else { return };
    let task = h.task();
    h.writer.execute_batch("CREATE TRIGGER inject BEFORE UPDATE ON tasks WHEN NEW.state='running' BEGIN SELECT RAISE(ABORT, 'running write failed'); END;").unwrap();
    assert!(Box::pin(h.start(task.clone())).await.is_err());
    assert_eq!(h.saved(&task).state, TaskState::Starting);
    assert_eq!(h.daemon.store.list_live_sessions().unwrap().len(), 1);
    h.writer.execute_batch("DROP TRIGGER inject;").unwrap();
    h.restart();
    h.daemon.recover_starting(Utc::now()).unwrap();
    assert_eq!(h.saved(&task).state, TaskState::Starting);
    assert_eq!(h.daemon.store.list_sessions_for_task(task.id).unwrap().len(), 1);
}

#[tokio::test]
async fn starting_resumed_session_update_failure_waits_for_unrecorded_agent() {
    let Some(mut h) = Harness::new() else { return };
    let task = h.task();
    assert!(matches!(Box::pin(h.start(task.clone())).await.unwrap(), Started::Launched));
    h.stop_panes(&task);
    let mut old = h.daemon.store.latest_session(task.id).unwrap().unwrap();
    let transcript = Path::new(old.transcript_path.as_ref().unwrap());
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(transcript, "").unwrap();
    let mut crashed = h.saved(&task);
    transitions::on_crash(&mut crashed, Some(&mut old), "test crash", None, &h.daemon.cfg.scheduler, Utc::now(), None);
    h.daemon.store.update_task(&crashed).unwrap();
    h.daemon.store.update_session(&old).unwrap();
    h.writer
        .execute_batch("CREATE TRIGGER inject BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT, 'session update failed'); END;")
        .unwrap();
    assert!(Box::pin(h.start(crashed)).await.is_err());
    assert_eq!(h.saved(&task).state, TaskState::Starting);
    assert_eq!(h.saved(&task).attempts, 2);
    assert_eq!((h.daemon.store.list_live_sessions().unwrap()).len(), 0);
    h.writer.execute_batch("DROP TRIGGER inject;").unwrap();
    h.restart();
    h.daemon.recover_starting(Utc::now()).unwrap();
    assert_eq!(h.saved(&task).state, TaskState::Starting);
    h.stop_panes(&task);
    h.daemon.recover_starting(Utc::now()).unwrap();
    assert_eq!(h.saved(&task).state, TaskState::Crashed);
    assert_eq!(h.saved(&task).attempts, 2);
}

#[tokio::test]
async fn background_setup_reload_persists_the_actual_target_before_session_write() {
    let Some(mut h) = Harness::new() else { return };
    let task = h.task();
    h.daemon.cfg.repo.setup = vec!["true".into()];
    assert_eq!(Box::pin(h.start(task.clone())).await.unwrap(), Started::Preparing);
    let old = h.target(&task);
    h.daemon.cfg.tmux.socket_name = Some(format!("powerqueue-test-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
    h.daemon.cfg.tmux.session_name = "reloaded".into();
    h.daemon.cfg.save(&h.daemon.paths).unwrap();
    h.daemon.reload_config().unwrap();
    h.tmux = h.daemon.rt.tmux.clone();
    // Lose the session row after creating its pane: restart recovery must
    // find that pane using the refreshed target, not the original claim.
    h.writer
        .execute_batch("CREATE TRIGGER inject BEFORE INSERT ON sessions BEGIN SELECT RAISE(ABORT, 'session insert failed'); END;")
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !h.daemon.rt.setups[&task.id].worker.is_finished() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    Box::pin(h.daemon.poll_setups(Utc::now())).await.unwrap();
    let target = h.target(&task);
    assert_ne!(target.socket_name, old.socket_name);
    assert_eq!(target.session_name, "reloaded");
    assert!(target.has_live_pane().unwrap());
    assert_eq!((h.daemon.store.list_sessions_for_task(task.id).unwrap()).len(), 0);
    h.writer.execute_batch("DROP TRIGGER inject;").unwrap();
    h.restart();
    h.daemon.recover_starting(Utc::now()).unwrap();
    assert_eq!(h.saved(&task).state, TaskState::Starting, "live agent must prevent another launch");
}
