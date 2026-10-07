//! `powerqueue service ...` against fake `systemctl` / `loginctl` /
//! `journalctl` scripts, so nothing touches the real user service manager.
//! Linux only (on macOS the manager is launchd).
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

/// A fake systemd: `systemctl` records every call in `calls.log` and keeps
/// the unit's active state in `state` (pid 4242 while active).
const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
dir="$(dirname "$0")"
echo "systemctl $*" >> "$dir/calls.log"
state="$(cat "$dir/state" 2>/dev/null || echo inactive)"
case "$*" in
  *" show "*)
    unit="$XDG_CONFIG_HOME/systemd/user/powerqueue.service"
    if [ -f "$unit" ]; then echo LoadState=loaded; echo UnitFileState=enabled; else echo LoadState=not-found; echo UnitFileState=; fi
    if [ "$state" = active ]; then echo ActiveState=active; echo SubState=running; echo MainPID=4242
    else echo ActiveState=inactive; echo SubState=dead; echo MainPID=0; fi ;;
  *"enable --now"*|*" start "*|*" restart "*) echo active > "$dir/state" ;;
  *"disable --now"*|*" stop "*) echo inactive > "$dir/state" ;;
esac
exit 0
"#;

const FAKE_LOGINCTL: &str = r#"#!/bin/sh
echo "loginctl $*" >> "$(dirname "$0")/calls.log"
case "$*" in show-user*) echo no ;; esac
exit 0
"#;

const FAKE_JOURNALCTL: &str = r#"#!/bin/sh
echo "journalctl $*" >> "$(dirname "$0")/calls.log"
echo "powerqueue[4242]: started"
"#;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for (name, body) in [("systemctl", FAKE_SYSTEMCTL), ("loginctl", FAKE_LOGINCTL), ("journalctl", FAKE_JOURNALCTL)] {
            let p = bin.join(name);
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = dir.path().join("pq/config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("config.toml"), format!("[repo]\npath = \"{}\"\n", dir.path().display())).unwrap();
        Self { dir }
    }

    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    fn unit(&self) -> PathBuf {
        self.dir.path().join("xdg/systemd/user/powerqueue.service")
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.bin().join("calls.log")).unwrap_or_default()
    }

    fn clear_calls(&self) {
        let _ = std::fs::remove_file(self.bin().join("calls.log"));
    }

    fn pq(&self) -> Command {
        let mut cmd = Command::cargo_bin("powerqueue").expect("binary builds");
        cmd.env("HOME", self.dir.path())
            .env("USER", "tester")
            .env("XDG_CONFIG_HOME", self.dir.path().join("xdg"))
            .env("POWERQUEUE_HOME", self.dir.path().join("pq"))
            .env("POWERQUEUE_SECRETS", "file")
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .env_remove("NO_COLOR")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("LINEAR_API_KEY")
            .env_remove("JEV_API_KEY");
        cmd
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

#[test]
fn print_changes_nothing() {
    let env = Env::new();
    env.pq()
        .args(["service", "install", "--print"])
        .assert()
        .success()
        .stdout(predicate::str::contains("KillMode=process").and(predicate::str::contains("POWERQUEUE_HOME=")));
    env.pq()
        .args(["service", "install", "--print", "--manager", "launchd"])
        .assert()
        .success()
        .stdout(predicate::str::contains("<key>AbandonProcessGroup</key><true/>"));
    assert!(!env.unit().exists());
    assert_eq!(env.calls(), "");
}

#[test]
fn install_status_restart_stop_uninstall() {
    let env = Env::new();

    env.pq()
        .args(["service", "install", "--env", "FOO=bar"])
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote").and(predicate::str::contains("started (pid 4242)")))
        .stdout(predicate::str::contains("install --linger"));
    let unit = read(&env.unit());
    assert!(unit.contains("KillMode=process"), "{unit}");
    assert!(unit.contains("Environment=\"FOO=bar\""), "{unit}");
    assert!(unit.contains(&format!("POWERQUEUE_HOME={}", env.dir.path().join("pq").display())), "{unit}");
    let calls = env.calls();
    assert!(calls.contains("systemctl --user daemon-reload"), "{calls}");
    assert!(calls.contains("systemctl --user enable --now powerqueue.service"), "{calls}");

    // Same environment: nothing to rewrite, nothing restarted.
    env.clear_calls();
    env.pq()
        .args(["service", "install", "--env", "FOO=bar"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unchanged").and(predicate::str::contains("already running")));
    assert!(!env.calls().contains("restart"), "{}", env.calls());

    env.pq().args(["service", "status"]).assert().success().stdout(
        predicate::str::contains("generated by powerqueue")
            .and(predicate::str::contains("yes (pid 4242"))
            .and(predicate::str::contains("survive a stop/restart")),
    );
    let out = env.pq().args(["--json", "service", "status"]).assert().success().get_output().stdout.clone();
    let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["installed"], true);
    assert_eq!(json["state"]["running"], true);
    assert_eq!(json["unit"]["keeps_sessions"], true);

    env.clear_calls();
    env.pq().args(["service", "restart"]).assert().success().stdout(predicate::str::contains("restarted (pid 4242)"));
    assert!(env.calls().contains("systemctl --user restart powerqueue.service"));

    env.pq().args(["service", "stop"]).assert().success().stdout(predicate::str::contains("stop requested"));
    env.pq().args(["service", "status"]).assert().code(3).stdout(predicate::str::contains("no (inactive/dead)"));
    env.pq().args(["service", "start"]).assert().success().stdout(predicate::str::contains("started (pid 4242)"));

    env.pq().args(["service", "logs", "-n", "5"]).assert().success().stdout(predicate::str::contains("started"));
    assert!(env.calls().contains("journalctl --user -u powerqueue.service --no-pager -n 5"));

    env.clear_calls();
    env.pq().args(["service", "uninstall"]).assert().success().stdout(predicate::str::contains("removed"));
    assert!(!env.unit().exists());
    assert!(env.calls().contains("systemctl --user disable --now powerqueue.service"));
    env.pq().args(["service", "uninstall"]).assert().success().stdout(predicate::str::contains("nothing to remove"));
    env.pq().args(["service", "status"]).assert().code(3).stdout(predicate::str::contains("not installed"));
}

#[test]
fn hand_written_unit_that_kills_sessions_is_guarded() {
    let env = Env::new();
    std::fs::create_dir_all(env.unit().parent().unwrap()).unwrap();
    // The unit the README used to suggest: no KillMode=process.
    std::fs::write(
        env.unit(),
        "[Unit]\nDescription=powerqueue daemon\n\n[Service]\nExecStart=%h/.cargo/bin/powerqueue run\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
    )
    .unwrap();
    std::fs::write(env.bin().join("state"), "active\n").unwrap();

    env.pq()
        .args(["service", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hand-written").and(predicate::str::contains("are killed when the service stops")))
        .stderr(predicate::str::contains("install --force"));
    env.pq().args(["service", "restart"]).assert().failure().stderr(predicate::str::contains("KillMode=process"));
    env.pq().args(["service", "stop"]).assert().failure().stderr(predicate::str::contains("--force"));
    env.pq()
        .args(["service", "install"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("differs from the generated file").and(predicate::str::contains("+KillMode=process")));
    assert!(!read(&env.unit()).contains("KillMode"));

    env.clear_calls();
    env.pq()
        .args(["service", "install", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("restarted to apply the new unit"));
    assert!(read(&env.unit()).contains("KillMode=process"));
    let calls = env.calls();
    let reload = calls.find("daemon-reload").expect("daemon-reload");
    let restart = calls.find("restart powerqueue.service").expect("restart");
    assert!(reload < restart, "the new KillMode must be loaded before the restart: {calls}");
}

#[test]
fn install_needs_init_and_linger_is_opt_in() {
    let env = Env::new();
    std::fs::remove_file(env.dir.path().join("pq/config/config.toml")).unwrap();
    env.pq().args(["service", "install"]).assert().failure().stderr(predicate::str::contains("powerqueue init"));
    assert!(!env.unit().exists());

    let env = Env::new();
    env.pq().args(["service", "install", "--linger"]).assert().success();
    assert!(env.calls().contains("loginctl enable-linger tester"), "{}", env.calls());
}
