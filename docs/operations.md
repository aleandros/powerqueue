# Operations

[Documentation](README.md) · [Project overview](../README.md)

## Running as a service

Run the daemon somewhere that survives your terminal. The easiest way is a
user service: a systemd user unit on Linux, a launchd agent on macOS.

```sh
powerqueue service install            # write the unit, enable it (starts at login), start it
powerqueue service install --linger   # Linux server: also keep it running after logout and start it at boot
powerqueue service status             # installed? running? unit still right?
powerqueue service logs -f            # the service manager's output (journalctl / launchd log files)
powerqueue service restart            # e.g. after `powerqueue update`
powerqueue service uninstall          # stop, disable, remove the file
```

`install` writes `~/.config/systemd/user/powerqueue.service` or
`~/Library/LaunchAgents/dev.powerqueue.plist`. The file runs this binary
(`<path> run`) with the `PATH` of the shell you install from, so the daemon
finds `git`, `tmux`, `gh` and the agent CLIs just as your shell does. Per-shell
version-manager directories such as fnm's `fnm_multishells/<pid>/bin` are
replaced by the stable directories they point to. The file also passes along
`POWERQUEUE_HOME`, `POWERQUEUE_SECRETS`, `XDG_{CONFIG,DATA,STATE}_HOME`,
`TMUX_TMPDIR`, `LANG` and `LC_ALL` when they are set; add more with
`--env KEY=VALUE`. API keys are never written to the file, so keep them in the
keychain or `secrets.toml` (`powerqueue secrets set linear`). `install` warns
when a key only exists in your shell's environment, when a tool is missing
from that `PATH`, and when the binary is a cargo build output that a rebuild
would replace.

The daemon restarts 10 s after a crash but stays stopped after
`powerqueue stop` or `service stop`. Stopping or restarting the service never
touches tmux, so running sessions carry on and the next daemon picks them up.
Under systemd this needs `KillMode=process`: the daemon starts the tmux server
inside the unit's cgroup, so the default kill mode would take every Claude
session down with it. launchd gets `AbandonProcessGroup` for the same reason.
`service status` and `doctor` flag a unit without it, and `stop`, `restart`
and `uninstall` refuse to run one until you regenerate it with
`service install --force` (or pass `--force`).

Re-run `powerqueue service install` after moving the binary or changing your
`PATH`. When the file would change it shows a diff and asks for `--force`,
then restarts a running service with the new file. `--print` shows the file
without installing anything (`--manager launchd|systemd` previews the other
platform), and `--no-start` enables it without starting it now. `install`
does not start a second daemon while one is already running in a terminal: it
tells you to `powerqueue stop` it, then `powerqueue service start`.

Without a service manager, use a tmux window:

```sh
tmux new-session -d -s pq-daemon 'powerqueue run'
```

## Secrets

Keys are looked up in this order:

1. environment: `LINEAR_API_KEY`, `GITHUB_TOKEN`, `JEV_API_KEY` (always win);
2. the OS keychain (macOS Keychain, Secret Service on Linux, Windows Credential Manager) under service `powerqueue`;
3. `<config>/secrets.toml`, mode 0600, used when no keychain is available.

`POWERQUEUE_SECRETS=file` forces the file backend (CI, containers, tests).
`powerqueue secrets list` shows which keys exist and where they come from
without printing values. Secrets are never logged.

## Logs and diagnostics

Paths follow XDG on every platform. `POWERQUEUE_HOME` (or `--home`) replaces
all three roots with `<HOME>/{config,data,state}`, which is how tests and
isolated instances run.

| What | Where |
|------|-------|
| config, `PRIORITY.md`, `secrets.toml` | `~/.config/powerqueue/` |
| database, worktrees | `~/.local/share/powerqueue/{powerqueue.db,worktrees/}` |
| logs, per-task dirs, lock/pid | `~/.local/state/powerqueue/{logs/,tasks/,daemon.lock,daemon.pid}` |
| service unit | `~/.config/systemd/user/powerqueue.service` (Linux), `~/Library/LaunchAgents/dev.powerqueue.plist` (macOS) |

- `powerqueue logs -f` tails `logs/powerqueue.log.<date>` (JSON lines by default); `--task ENG-123` filters; `--events` replays the DB timeline.
- `powerqueue service logs` shows what systemd/launchd captured, which is where a daemon that fails at startup leaves its error; `service status` says whether the unit still fits this binary.
- `powerqueue task show ENG-123` prints the task's timeline, sessions and usage.
- `powerqueue task output ENG-123` shows the last screen of the pane; `attach` opens it.
- `powerqueue dashboard --once` prints the dashboard frame as plain text (`--json` for the raw snapshot, with `ledgers.by_provider.<p>`, `cooldowns` and `next_model`); paste it when the live dashboard looks wrong.
- `powerqueue doctor --json` is what to paste into bug reports.

Symptoms and fixes: [docs/troubleshooting.md](troubleshooting.md).
Internals: [docs/architecture.md](architecture.md).
