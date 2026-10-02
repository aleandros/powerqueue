# powerqueue

[![CI](https://github.com/aleandros/powerqueue/actions/workflows/ci.yml/badge.svg)](https://github.com/aleandros/powerqueue/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 2024](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](Cargo.toml)

powerqueue turns Linear tickets into Claude Code sessions. It is a single Rust
binary that runs as a daemon on your machine. It polls Linear, scores tickets
with rules you write in Markdown, gives each task its own git worktree and tmux
window, and launches an interactive Claude Code session in it. It learns how
much each kind of task costs, restarts crashed sessions, paces model usage
across your subscription period (Fable is held back for critical work), cleans
up when a task is done, and ships a live dashboard plus a `doctor` command that
tells you what is wrong and how to tune it.

## How it flows

```text
 Linear (Todo, labels) ──poll──▶ queue (SQLite) ──pick──▶ git worktree ──▶ tmux window
                                 PRIORITY.md score        pq/<key>          claude --session-id …
                                 budget policy                                   │
                                                                                 ▼
 Linear state + comment ◀── cleanup ◀── DONE / BLOCKED ◀── Claude Code hooks ──▶ powerqueue hook
                            push, rm worktree,            + transcript usage     (hook_events table)
                            close window
```

## Requirements

| Tool | Version | Notes |
|------|---------|-------|
| git | any recent | worktrees are created with `git worktree` |
| tmux | ≥ 3.2 | one session (`powerqueue`), one window per task |
| Claude Code | ≥ 2.1.2xx, logged in | `claude auth status` must succeed |
| Linear API key | personal key | stored in the OS keychain by `init` |
| Jev (TypeSafe) API key | optional | adds a model-based urgency score |

Linux and macOS are supported. Windows is untested.

## Install

One line, no toolchain needed. The script detects your OS and architecture and
drops the binary in `/usr/local/bin` (or `~/.local/bin` as a fallback):

```sh
curl -fsSL https://raw.githubusercontent.com/aleandros/powerqueue/main/install.sh | sh
```

**Update**: run the same command again. It replaces the binary atomically, so
a running daemon keeps working until you restart it (`powerqueue stop`, then
`powerqueue run`). Pin a version with `POWERQUEUE_VERSION=v0.1.0`, or choose
the directory with `POWERQUEUE_INSTALL_DIR=~/bin`.

Other ways:

```sh
# with a Rust toolchain
cargo install --git https://github.com/aleandros/powerqueue

# from a checkout
cargo install --path .
```

Pre-built binaries for Linux (x86_64, aarch64) and macOS (x86_64, arm64) are
attached to every [GitHub release](https://github.com/aleandros/powerqueue/releases),
with SHA-256 sums. Releases are cut automatically when `version` in
`Cargo.toml` changes on `main`.

Project site: <https://aleandros.github.io/powerqueue/>

## Quick start

```sh
cd ~/code/my-repo
powerqueue init --team ENG          # keys, repo, Linear team, PRIORITY.md
powerqueue doctor                   # checks git/tmux/claude, keys, config
powerqueue run                      # daemon, foreground
```

Run the daemon somewhere that survives your terminal. In a tmux window:

```sh
tmux new-session -d -s pq-daemon 'powerqueue run'
```

Or with launchd (macOS, `~/Library/LaunchAgents/dev.powerqueue.plist`):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.powerqueue</string>
  <key>ProgramArguments</key><array>
    <string>/usr/local/bin/powerqueue</string><string>run</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict></plist>
```

Or with systemd (Linux, `~/.config/systemd/user/powerqueue.service`):

```ini
[Unit]
Description=powerqueue daemon

[Service]
ExecStart=%h/.cargo/bin/powerqueue run
Restart=on-failure
Environment=PATH=%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin

[Install]
WantedBy=default.target
```

Then watch and interact:

```sh
powerqueue dashboard                # live TUI (aliases: ui, top)
powerqueue status                   # one-shot table
powerqueue attach ENG-123           # jump into the task's tmux window
powerqueue add "Fix flaky test" -c high   # manual task, no Linear needed
```

## Command reference

Global flags work on every command.

| Flag | Meaning |
|------|---------|
| `-v`, `-vv` | debug / trace logging on stderr |
| `-q`, `--quiet` | only print errors |
| `--home DIR` | base directory for config/data/state (env `POWERQUEUE_HOME`) |
| `--json` | machine-readable output where supported (status, add, task, priority, budget, linear, doctor, config, `logs --events`) |
| `--no-color` | disable colours (env `NO_COLOR`) |

### Setup and daemon

| Command | What it does |
|---------|--------------|
| `init [--repo PATH] [--team KEY]... [--linear-key K] [--jev-key K] [--no-linear] [--permission-mode MODE] [--non-interactive] [--reconfigure] [--force]` | guided first-time setup; writes `config.toml` and `PRIORITY.md`, stores keys; `--no-linear` sets `linear.enabled = false` (manual tasks only); `--permission-mode` picks `acceptEdits` (default), `auto`, `bypassPermissions`, `dontAsk`, `plan` or `default`; on an existing install the menu offers "Change settings", and `--reconfigure` walks the editable settings (repository, default branch, Linear team and states, concurrency, permission mode, weekly budget, reset anchor) with the current values as defaults, keeping keys and every other key |
| `run [--once] [--offline]` | run the scheduler in the foreground; `--once` does one pass; `--offline` skips Linear |
| `stop` | ask the running daemon to exit (sessions keep running in tmux) |
| `dashboard` (`ui`, `top`) | live TUI |
| `status [-a]` (`ls`) | one-shot table; `-a` includes completed/failed/cancelled |
| `doctor [--fix] [--offline]` | diagnostics and tuning advice; `--fix` applies safe repairs |
| `logs [-f] [-n N] [-t TASK] [-l LEVEL] [--events]` | read the daemon log (default 200 lines); `--events` shows the DB timeline instead; `-f` follows either |
| `completions <shell>` | shell completions (bash, elvish, fish, powershell, zsh) |

### Tasks

| Command | What it does |
|---------|--------------|
| `add "title" [-d DESC\|-] [-c CRIT] [-m MODEL] [-k KEY] [-l LABEL]... [--paused]` | enqueue a manual task; `-d -` reads stdin |
| `task show <task>` | details and timeline |
| `task list [-a]` | same as `status` |
| `task complete <task> [-s SUMMARY]` | mark completed (Claude calls this from inside the session) |
| `task block <task> [-r REASON]` | mark blocked / needs a human |
| `task cancel <task>` | cancel and release resources |
| `task pause <task>` | do not schedule; a running session stops after its turn |
| `task resume <task>` | resume a paused or needs-attention task |
| `task retry <task>` | re-queue a failed, cancelled or completed task (also crashed, throttled, paused, needs-attention) |
| `task explain <task>` | current score and model decision, with reasons |
| `task model <task> <fable\|opus\|sonnet\|haiku\|auto>` | force (or clear) the model for the next attempt; the policy may still downgrade it when the tier is out of budget |
| `task output <task> [-n LINES]` | last screen of the task's tmux pane (default 60 lines) |
| `task send <task> "message"` | type a message into the running session |
| `attach [<task>] [--print]` | open the task's tmux window; no task = the powerqueue session |

`<task>` is a key (`ENG-123`, `manual-1a2b3c4d`), a full id, or an id prefix.
`pause`, `resume`, `cancel` and `retry` are queued for the daemon; with no
daemon running (no heartbeat for 30 s) they are applied directly when the
state machine allows it. `complete` and `block` always write directly.

### Priority rules

| Command | What it does |
|---------|--------------|
| `priority show` | print the parsed rules |
| `priority check` | validate `PRIORITY.md` and report problems with line numbers |
| `priority edit` | open `PRIORITY.md` in `$EDITOR` |
| `priority explain <task>` | show how the rules score one task |
| `priority path` | print the path of the rules file |

### Budget

| Command | What it does |
|---------|--------------|
| `budget show` | period/window spend per tier and what the policy would allow |
| `budget set-reset <when>` | record the period reset instant from `/usage` (RFC 3339 or `in 3d4h`) |
| `budget set-observed <percent>` | calibrate pacing with the percentage from `/usage` (e.g. `43%`) |
| `budget clear-limits` | forget rate-limit cooldowns |
| `budget estimate [<task>]` | the cost estimator's view of a task (or all history) |

### Linear

| Command | What it does |
|---------|--------------|
| `linear teams` | teams visible to the API key |
| `linear states <team>` | workflow states of a team |
| `linear test` | verify the key (prints the viewer) |
| `linear sync [--apply]` | fetch queued issues and print what would change; `--apply` applies it |

### Config and secrets

| Command | What it does |
|---------|--------------|
| `config show` | effective configuration as TOML |
| `config path` | config/data/state paths |
| `config get <key>` | one value by dotted key (`claude.permission_mode`, `budget.models.fable.share`, `linear.team_keys`) as TOML; `--json` for JSON |
| `config set <key> <value>` | change one key; `<value>` is TOML (`3`, `true`, `["ENG","OPS"]`) or a bare string (`auto`); the result is validated before anything is written, comments in `config.toml` survive, and a running daemon is asked to reload (new sessions use the new value) |
| `config unset <key>` | remove a key so its default applies again |
| `config edit` | open `config.toml` in `$EDITOR` |
| `config validate` | validate `config.toml` and the repo's `.powerqueue.toml` |
| `secrets set <linear\|jev> [value]` | store a key (prompts if omitted) |
| `secrets unset <name>` | remove a key |
| `secrets list` | which keys are configured and where they come from |
| `hook --task ID [--session SID] --event EVENT` | internal; called by Claude Code hooks (hidden) |

## How a task is run

1. The daemon picks the highest-scoring schedulable task and asks the budget
   policy for a model (see [docs/budget.md](docs/budget.md)).
2. It creates `<worktree_root>/<slug>` on branch `pq/<slug>` (configurable)
   and runs `repo.setup` commands there.
3. It writes a per-task directory under `<state>/tasks/<task-id>/`:

   | File | Content |
   |------|---------|
   | `prompt.md` | the brief Claude receives as its first message |
   | `settings.json` | Claude Code hooks pointing back at `powerqueue hook` |
   | `launch.sh` | the exact command line (mode 0700); re-run it by hand to reproduce |
   | `env` | environment for the session (mode 0600): `claude.env` plus `POWERQUEUE_TASK_ID`, `POWERQUEUE_TASK_KEY`, `POWERQUEUE_SESSION_ID` |

4. Unless `claude.trust_workspace = false`, the repository root and the worktree
   are marked trusted in Claude Code's `~/.claude.json` (the key the official
   docs say to set by hand), so the interactive session starts straight away.
5. A tmux window named after the task's slug (`eng-123`) runs `sh launch.sh`, which
   `exec`s `claude --session-id <uuid> --model <tier> --permission-mode <mode> --settings settings.json --name <key> …`
   with the prompt (read from `prompt.md`) as the last argument, so it is the
   first message. The session is interactive, so `powerqueue attach` drops
   you into it at any time.
6. Hooks (`SessionStart`, `Stop`, `StopFailure`, `SessionEnd`, `Notification`,
   `PreCompact`, `UserPromptSubmit`) call `powerqueue hook`, which stores the
   payload in SQLite. The daemon drains these, tails the transcript JSONL for
   token usage (deduplicated by message id), and samples CPU/RSS.

### The prompt protocol

The prompt (`src/session/launcher.rs::build_prompt`) contains the key and
title, the description, the Linear link, the working rules (stay on the
branch, commit as you go, do not push) and the completion protocol:

- **Done**: run `powerqueue task complete <id> --summary "..."` and print
  `[[POWERQUEUE:DONE]]` as the last line of the final message. Either one on
  its own completes the task; text after the marker becomes the summary.
- **Blocked**: run `powerqueue task block <id> --reason "..."` and print
  `[[POWERQUEUE:BLOCKED]]`. The task moves to `needs_attention` and waits for a
  human (`powerqueue attach`, then `task resume`). A final message that reads
  like a question (ends with `?`, or says "should I", "let me know", ...) is
  treated the same way.
- Attempts after the first add an `## Attempt N` section saying how the
  previous one ended.
- A turn that ends without a marker leaves the task `idle`. After
  `scheduler.idle_timeout_secs` it is nudged once; after another timeout it is
  marked `needs_attention`.
- A dead tmux pane without a completion marker means `crashed`. The daemon waits
  `scheduler.restart_backoff_secs[attempt]` and relaunches with
  `claude --resume <same session id>`, so context is kept. After
  `scheduler.max_attempts` the task is `failed`.
- A `StopFailure` with `rate_limit`, `overloaded`, `usage_limit` or `quota`
  puts the tier on cooldown and the task in `throttled`.

### Cleanup

On completion the daemon pushes the branch, removes the worktree, closes the
tmux window, moves the Linear issue to `linear.done_state` and posts a comment
with the summary. Unpushed work is never deleted: if the push fails or there is
no remote, the worktree stays and an event says why. Failed tasks keep their
worktree when `cleanup.keep_failed` is true.

## Configuration

`powerqueue init` writes `config.toml` to the config directory (see
[Paths](#logs-and-diagnostics)). Every key is optional; defaults are shown.
Unknown keys are rejected. `powerqueue config validate` and `doctor` explain
problems in plain language.

### `[repo]`

| Key | Default | Meaning |
|-----|---------|---------|
| `path` | `""` (required) | main checkout; worktrees are created from it |
| `default_branch` | detect | base for new worktrees (`origin/HEAD`, then `main`/`master`) |
| `worktree_root` | `<data>/worktrees/<repo-name>` | where worktrees live |
| `branch_template` | `"pq/{key}"` | `{key}` = task key slug, `{id}` = short task id |
| `fetch_before_start` | `true` | `git fetch` before creating a worktree |
| `setup` | `[]` | commands run (`sh -c`) in a fresh worktree before Claude starts |

### `[linear]`

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | `true` | poll Linear at all |
| `team_keys` | `[]` | team keys to pull from; empty = every team the key can see |
| `assignee` | none | only issues assigned to this user id, or `"me"` |
| `queued_states` | `["Todo"]` | workflow state names that mean "ready for the agent" |
| `required_labels` | `[]` | issues must carry one of these labels |
| `excluded_labels` | `["no-agent"]` | issues with any of these are ignored |
| `in_progress_state` | `"In Progress"` | state set when a session starts |
| `done_state` | `"In Review"` | state set on completion |
| `blocked_state` | none | state set when a task fails permanently or is blocked |
| `post_comments` | `true` | post progress comments on the issue |
| `poll_interval_secs` | `60` | how often to poll |
| `max_issues` | `100` | cap per poll |
| `endpoint` | `https://api.linear.app/graphql` | GraphQL endpoint (tests) |

### `[priority]` and `[priority.jev]`

| Key | Default | Meaning |
|-----|---------|---------|
| `file` | `<config>/PRIORITY.md` | rules file |
| `live_reload` | `true` | watch the file and re-score tasks on change |
| `age_boost_per_hour` | `2.0` | points a waiting task gains per hour so nothing starves (capped at 200) |
| `jev.enabled` | `false` | score tickets with Jev |
| `jev.endpoint` | `https://api.typesafe.ai/v1/systemone` | API endpoint |
| `jev.model` | `"jev-latest"` | model name |
| `jev.rescore_on_change` | `true` | re-score when title/description/labels change, else cache |
| `jev.weight` | `300.0` | points added for a normalised Jev score of 1.0 |

### `[scheduler]`

| Key | Default | Meaning |
|-----|---------|---------|
| `max_concurrent` | `2` | parallel sessions |
| `max_attempts` | `3` | attempts before a task is `failed` |
| `tick_secs` | `5` | main loop period |
| `idle_timeout_secs` | `600` | silent after a turn without a marker: nudge once, then `needs_attention` |
| `stale_session_secs` | `1800` | no transcript growth or hook events while running = hung |
| `restart_backoff_secs` | `[30, 120, 600]` | backoff after a crash, per attempt (last value repeats) |
| `max_session_secs` | `14400` | wall-clock cap per attempt; `0` disables |
| `resource_sample_secs` | `30` | CPU/RSS sampling interval |

### `[claude]`

| Key | Default | Meaning |
|-----|---------|---------|
| `binary` | `"claude"` | Claude Code executable |
| `permission_mode` | `"acceptEdits"` | `--permission-mode`; `auto` is the unattended choice, `bypassPermissions` never asks at all |
| `effort` | none | `--effort` value |
| `extra_args` | `[]` | flags appended verbatim |
| `allowed_tools` | `[]` | extra `--allowedTools` patterns |
| `append_system_prompt` | none | appended to the system prompt for every task |
| `fallback_models` | `[]` | passed as `--fallback-model` |
| `trust_workspace` | `true` | mark the repository and each worktree as trusted in Claude Code's `~/.claude.json` before launching, so sessions never wait on the workspace-trust dialog |
| `env` | `{}` | environment variables for the session |

Allowed permission modes: `default`, `manual`, `acceptEdits`, `plan`, `auto`,
`dontAsk`, `bypassPermissions`. `powerqueue init` offers them with a one-line
description each (`--permission-mode MODE` skips the prompt), and
`powerqueue config set claude.permission_mode auto` changes the mode later.

Every session also gets `--allowedTools "Bash(powerqueue task *)"` so the
completion protocol never waits on a permission prompt. In `acceptEdits` mode
other shell commands (tests, `git commit`, package installs) still prompt and
leave the task in `needs_attention` until you attach and answer. For truly
unattended runs (a VPS, a daemon nobody watches) pick `auto`, where Claude
Code's own classifier approves routine commands, or `bypassPermissions`,
and/or pre-approve what your repo needs in `allowed_tools`, for example
`["Bash(git *)", "Bash(cargo *)", "Bash(npm test*)"]`.

### `[budget]` and `[budget.models.<tier>]`

| Key | Default | Meaning |
|-----|---------|---------|
| `period_hours` | `168` | usage period length (subscriptions reset weekly) |
| `period_anchor` | none | RFC 3339 start of a period; set with `budget set-reset` |
| `window_hours` | `5` | rolling short window |
| `period_weighted_tokens` | `80000000` | weighted tokens the period may consume |
| `window_weighted_tokens` | `12000000` | weighted tokens the window may consume |
| `default_model` | `"sonnet"` | model when nothing else applies |
| `low_model` | `"sonnet"` | model for `low` tasks |
| `safety_margin` | `0.05` | fraction of the remaining budget kept unspent (0 to 0.5) |
| `endgame_fraction` | `0.8` | after this fraction of the period, reserved capacity is released |
| `rate_limit_cooldown_mins` | `30` | pause a tier after a `rate_limit` error (never past the period end) |

Per tier (`[budget.models.fable]`, `.opus`, `.sonnet`, `.haiku`):

| Key | fable | opus | sonnet | haiku | Meaning |
|-----|-------|------|--------|-------|---------|
| `share` | `0.25` | `0.35` | `0.35` | `0.05` | fraction of `period_weighted_tokens` |
| `min_criticality` | `critical` | `high` | `low` | `low` | minimum criticality early in the period |
| `relax_after_fraction` | `0.5` | `0.3` | `0.0` | `0.0` | when under-spent tiers open up one level |
| `weight` | `5.0` | `3.0` | `1.0` | `0.2` | cost weight relative to Sonnet |
| `enabled` | `true` | `true` | `true` | `true` | tier may be used |

Enabled shares must sum to at most 1.0.

### `[cleanup]`

| Key | Default | Meaning |
|-----|---------|---------|
| `remove_worktree` | `true` | remove the worktree after completion |
| `push_branch` | `true` | push before removal so work is not lost |
| `delete_branch` | `false` | delete the local branch afterwards |
| `keep_failed` | `true` | keep worktrees of failed tasks |
| `run` | `[]` | commands run (`sh -c`) in the worktree before removal |
| `close_tmux_window` | `true` | kill the window; otherwise it stays with the final output |

### `[tmux]`

| Key | Default | Meaning |
|-----|---------|---------|
| `binary` | `"tmux"` | tmux executable |
| `session_name` | `"powerqueue"` | session hosting one window per task |
| `socket_name` | none | tmux `-L` socket name |
| `remain_on_exit` | `true` | keep dead panes so crashes can be inspected |

### `[logging]`

| Key | Default | Meaning |
|-----|---------|---------|
| `level` | `"info"` | `tracing` filter for the log file, e.g. `info,powerqueue=debug` |
| `keep_days` | `14` | rotated daily files to keep |
| `json` | `true` | JSON lines (true) or text (false) in the file |

### Per-repository overrides: `.powerqueue.toml`

A `.powerqueue.toml` in the repository root overrides a subset of the global
config for that repo. Lists replace; `instructions` and
`claude.append_system_prompt` are appended to the global value.

```toml
setup = ["npm ci"]
default_branch = "develop"
branch_template = "agent/{key}"
instructions = "Run `npm test` before you finish. Never touch migrations."

[cleanup]
remove_worktree = false     # also: push_branch, delete_branch, keep_failed, run, close_tmux_window

[claude]
permission_mode = "plan"    # also: effort, extra_args, allowed_tools, append_system_prompt
```

## Priority rules (PRIORITY.md)

Rules live in a Markdown file so they stay readable anywhere. Sections are `##`
headings; rules are bullets. A ticket's criticality is the first section
(Critical → High → Normal → Low) with a matching rule.

```markdown
## Critical
- label: incident
- priority: urgent and label: customer

## Low
- label: chore

## Scoring
- +40 if label: customer
- -30 if estimate > 8

## Overrides
- ENG-123: critical
- ENG-200: model = opus
- ENG-300: skip

## Models
- critical: fable
- high: opus
- normal: sonnet
- low: sonnet
```

The file is re-read whenever it changes. `## Models` and `KEY: model = x` are
preferences handed to the budget policy; only `task model` and `add --model`
set a hard override, and even that is downgraded when the tier is out of
budget. Full grammar, evaluation order and idioms:
[docs/priority.md](docs/priority.md).

## Budget pacing

Every API call's usage is weighted (`input + 1.25·cache_write + 0.1·cache_read + 5·output`),
multiplied by the tier weight, and charged against a period budget and a
rolling window. Each tier has a share of the period and a minimum criticality.
Early in the period Fable only serves `critical` tasks; if a tier is under-spent
past `relax_after_fraction`, one level lower qualifies; in the end game two
levels. The predicted cost must also fit the tier's remaining share, the
overall period budget and the window. If nothing is eligible the task is
`throttled` with a retry time.
Calibrate with `budget set-reset` and `budget set-observed`; `doctor` tells you
when to. Details and worked examples: [docs/budget.md](docs/budget.md).

## Secrets

Keys are looked up in this order:

1. environment: `LINEAR_API_KEY`, `JEV_API_KEY` (always win);
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

- `powerqueue logs -f` tails `logs/powerqueue.log.<date>` (JSON lines by default); `--task ENG-123` filters; `--events` replays the DB timeline.
- `powerqueue task show ENG-123` prints the task's timeline, sessions and usage.
- `powerqueue task output ENG-123` shows the last screen of the pane; `attach` opens it.
- `powerqueue doctor --json` is what to paste into bug reports.

Symptoms and fixes: [docs/troubleshooting.md](docs/troubleshooting.md).
Internals: [docs/architecture.md](docs/architecture.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Agents and humans should also read
[AGENTS.md](AGENTS.md). Security issues: [SECURITY.md](SECURITY.md).

## License

MIT. See [LICENSE](LICENSE).
