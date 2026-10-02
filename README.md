# powerqueue

[![CI](https://github.com/aleandros/powerqueue/actions/workflows/ci.yml/badge.svg)](https://github.com/aleandros/powerqueue/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 2024](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](Cargo.toml)

**Get the most out of the Claude subscription you already pay for.** powerqueue
is an autonomous work queue that turns Linear tickets into Claude Code sessions
and keeps them running around the clock, on a cheap VPS or on your own machine.

You are paying for a weekly allowance that mostly goes unused: the 5-hour
windows reset while you sleep, and the strongest model sits idle until
something urgent comes up. powerqueue is a single Rust binary that puts that
capacity to work. It polls Linear, scores tickets with rules you write in
Markdown, gives each task its own git worktree and tmux window, launches an
interactive Claude Code session in it, and paces model usage so the week's
budget is spent on the work that matters most: Fable is held back for critical
tickets early in the period and released before it would go to waste.

The whole stack is your subscription plus one small box. No API keys, no
per-token bills, no orchestration platform to host. A VPS with a couple of
gigabytes of RAM runs several sessions side by side, and your laptop works just
as well. Setup is `init`, `run`, done; everything else is optional tuning.

Along the way it learns what each kind of task costs, restarts crashed sessions
with their context intact, cleans up when a task is done, and ships a live
dashboard plus a `doctor` command that tells you what is wrong and which knob
to turn.

## Why this is cheap

| You need | What it costs |
|----------|---------------|
| A Claude subscription (Pro or Max) | you already have it; powerqueue only spends the allowance you are not using |
| One machine that stays on | the smallest VPS you can rent, or your laptop; it runs git, tmux and Claude Code, nothing heavier |
| Linear | optional; `powerqueue add` queues work by hand |

That is the whole bill. Sessions run through your normal Claude Code login, so
usage counts against the subscription, not an API account. The budget policy
keeps you inside the weekly limit and the 5-hour window instead of hitting
rate limits, and `doctor` tells you when the pacing is off.

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
| Codex CLI | optional | `codex login status` must succeed; adds OpenAI's weekly budget (see [Providers](#providers)) |
| Antigravity CLI (`agy`) | optional, experimental | signed in to Google AI Pro/Ultra |
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

### On a VPS

The cheapest way to run powerqueue all week is a small Linux box that stays on.
The setup is the same as on a laptop, plus a one-time login:

```sh
# 1. tools: git, tmux, Node (for Claude Code)
sudo apt install -y git tmux nodejs npm
npm install -g @anthropic-ai/claude-code

# 2. log in with your subscription (prints a URL; open it on any device, paste the code back)
claude

# 3. powerqueue, your repo, and the guided setup
curl -fsSL https://raw.githubusercontent.com/aleandros/powerqueue/main/install.sh | sh
git clone git@github.com:you/your-repo.git ~/code/your-repo
cd ~/code/your-repo && powerqueue init --team ENG --permission-mode auto
```

Pick the `auto` permission mode (or `bypassPermissions`) during `init` so
sessions never wait for a human; on a laptop where you are around,
`acceptEdits` is the safer choice. Then keep the daemon alive with systemd
(below) and check on it from anywhere with `ssh box -t powerqueue dashboard`
or `powerqueue attach ENG-123`.

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
powerqueue dashboard                # live TUI (aliases: ui, top); --once prints one frame
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
| `init [--repo PATH] [--team KEY]... [--linear-key K] [--jev-key K] [--no-linear] [--permission-mode MODE] [--provider P]... [--non-interactive] [--reconfigure] [--force]` | guided first-time setup; writes `config.toml` and `PRIORITY.md`, stores keys; `--no-linear` sets `linear.enabled = false` (manual tasks only); `--permission-mode` picks `acceptEdits` (default), `auto`, `bypassPermissions`, `dontAsk`, `plan` or `default`; when `codex` / `agy` are on PATH it asks whether to run tasks on them too (`--provider codex --provider gemini` answers yes non-interactively and warns when the CLI is not logged in); on an existing install the menu offers "Change settings", and `--reconfigure` walks the editable settings (repository, default branch, Linear team and states, concurrency, permission mode, weekly budget, reset anchor, providers) with the current values as defaults, keeping keys and every other key |
| `run [--once] [--offline]` | run the scheduler in the foreground; `--once` does one pass; `--offline` skips Linear |
| `stop` | ask the running daemon to exit (sessions keep running in tmux) |
| `dashboard [--once] [--ascii]` (`ui`, `top`) | live TUI: task table, one budget block per enabled provider (period, window, cooldown, a gauge per model), a header with the compact per-provider summary (`cl 34/12%  cx 17/–` = period/window spent) and `next <model>` (what the policy would run now); needs an interactive terminal (exit 2 otherwise). `--once` prints one frame as text and exits (works in pipes; with `--json` prints the snapshot); `--ascii` uses `*`/`>`/`#` and `+-\|` borders (automatic when the locale is not UTF-8) |
| `status [-a]` (`ls`) | one-shot table; `-a` includes completed/failed/cancelled |
| `doctor [--fix] [--offline]` | diagnostics and tuning advice, per enabled provider (binary and version, logged in, model shares, period anchor source, probe freshness, top-model pacing, window pressure); `--fix` applies safe repairs |
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
| `task model <task> <model\|auto>` | force (or clear) the model for the next attempt (`fable`, `opus`, `sonnet`, `haiku`, or another provider's model such as `gpt-6.1-sol`, `gemini-3-pro`, `codex:<name>`); the policy may still downgrade it within the same provider when the model is out of budget |
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
| `priority show` | print the parsed rules (model preference lists joined with ` \| `) |
| `priority check` | validate `PRIORITY.md`, report problems with line numbers and print the `## Models` lists |
| `priority edit` | open `PRIORITY.md` in `$EDITOR` |
| `priority explain <task>` | show how the rules score one task |
| `priority path` | print the path of the rules file |

### Budget

| Command | What it does |
|---------|--------------|
| `budget show` | per enabled provider: period/window spend per model, observed usage and its age, anchor source and cooldowns; then what the policy would run now (`--json`: `{"providers": {...}, "next": {...}}`) |
| `budget probe [--provider P]` | ask providers for their remaining allowance now (Codex `app-server`, Claude's status line, `agy /usage`) and store it; exits 1 when a probe fails |
| `budget set-reset <when> [--provider P]` | record a provider's period reset instant (from `/usage` in Claude Code; RFC 3339 or `in 3d4h`); default provider `claude` |
| `budget set-observed <percent> [--provider P]` | calibrate a provider's pacing with the percentage it shows (e.g. `43%`) |
| `budget clear-limits [--provider P]` | forget a provider's rate-limit cooldowns |
| `budget estimate [<task>]` | the cost estimator's view of a task (or all history) |

`--provider` accepts `claude`, `codex` or `gemini` (`antigravity`/`agy` are aliases of `gemini`).

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
| `hook --task ID [--session SID] --event EVENT [--provider P] [PAYLOAD]` | internal; called by agent CLI hooks (hidden); `--event StatusLine` is the Claude status line (stores rate limits, prints a short status); `PAYLOAD` replaces stdin (Codex `notify`) |

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

### Providers

powerqueue can run tasks on three coding-agent CLIs, each paid for by its own
subscription and paced against its own weekly budget:

| Provider | CLI | What it needs | Shipped models (most capable first) |
|----------|-----|---------------|-------------------------------------|
| `claude` (default, always on) | Claude Code (`claude`) | logged in with your Pro/Max subscription (`claude auth status`) | `fable`, `opus`, `sonnet`, `haiku` |
| `codex` | OpenAI Codex CLI (`codex`) | `npm install -g @openai/codex`, then `codex login` with a ChatGPT Plus/Pro/Business account | `gpt-6.1-sol`, `gpt-6-astra`, `gpt-6-luna` |
| `gemini` (experimental) | Google Antigravity CLI (`agy`) | install `agy` and sign in with a Google AI Pro/Ultra account | `gemini-3-pro`, `gemini-3-flash` |

Enable a provider with `init` (it asks when the binary is on PATH; `init
--provider codex` / `init --reconfigure` do the same), or by hand:
`powerqueue config set budget.providers.codex.enabled true`. Each provider has
its own `[budget.providers.<p>]` table (period, window, model shares), its own
usage probe, its own rate-limit cooldowns and its own ledger; `budget show`
and the dashboard print one block per enabled provider, and `doctor` checks
each one (binary and version, logged in, shares, anchor, probe freshness).

Which provider a task lands on: a `task model` / `add --model` override is a
hard choice (it never crosses providers; the CLI warns when that provider is
disabled). Otherwise the task's **preference list** is tried in order: the
`## Overrides` model list in `PRIORITY.md` (`ENG-1: model = fable |
gpt-6.1-sol`), else the `## Models` entry for its criticality (`critical:
fable | gpt-6.1-sol`), each alternative through its provider's downgrade
chain. When nothing on the list is eligible, the first eligible model in
`budget.provider_order` (default `["claude", "codex", "gemini"]`) runs it, so
a provider that is out of budget or rate-limited falls back to the next one.
Model names infer their provider (`gpt-*` → codex, `gemini-*` → gemini);
anything else uses the explicit `codex:<name>` form.

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

### `[codex]`

Settings for OpenAI Codex CLI sessions (tasks whose model belongs to Codex,
e.g. `gpt-6.1-sol`). Each session runs

```
codex -C <worktree> -m <model> <approval flags> \
  -c 'notify=["<powerqueue>","hook","--provider","codex",...,"--event","Notify"]' \
  [-c model_reasoning_effort="<e>"] [-c 'projects={ "<worktree>" = { trust_level = "trusted" } }'] \
  --add-dir <data dir> --add-dir <state dir> [--add-dir <repo>/.git] <extra_args> "<prompt>"
```

and a crash restart runs `codex resume <thread id> ...` with the same flags.
Codex picks its own thread id: the daemon finds it in the session's rollout
(`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`, matched by working
directory; `CODEX_HOME` from `codex.env`, else the daemon's environment, else
`~/.codex`), records it as the session's `agent_session_id` and tails the
rollout for token usage. Completion arrives through Codex's `notify` program
after every turn (no files are written to the worktree); throttling errors in
the rollout (`You've hit your usage limit`, `Quota exceeded`, ...) put Codex on
its rate-limit cooldown.

The `workspace-write` sandbox only lets the session write inside the
worktree, so powerqueue adds its data and state directories (for `powerqueue
task complete`) and the main checkout's `.git` (for commits) with `--add-dir`.
The `[[POWERQUEUE:DONE]]` marker in the final message completes the task even
if the completion command fails.

| Key | Default | Meaning |
|-----|---------|---------|
| `binary` | `"codex"` | Codex CLI executable |
| `approval` | `"workspace-write"` | `workspace-write` (`-a never -s workspace-write`), `approve-for-me` (`--approve-for-me`), `yolo` (`--dangerously-bypass-approvals-and-sandbox`) or `on-request` (`-a on-request -s workspace-write`) |
| `reasoning_effort` | `"high"` | `-c model_reasoning_effort=…` (`low`, `medium`, `high`, `xhigh`, `max`, `ultra`); unset to leave Codex's default |
| `extra_args` | `[]` | flags appended verbatim |
| `env` | `{}` | environment variables for the session (`CODEX_HOME` here is also where the daemon looks for rollouts) |
| `trust_workspace` | `true` | mark the worktree as a trusted project for the session (`-c projects={...}`, an inline table because Codex splits dotted `-c` keys on `.`) so Codex never stops at its folder-trust dialog |

### `[gemini]` (experimental)

Settings for Google's Antigravity CLI (`agy`, the CLI behind Google AI
Pro/Ultra). **Experimental:** the flags, hook file and transcript layout come
from vendor docs and community reports and were not verified against a real
`agy`; `doctor` says so while `budget.providers.gemini.enabled` is true. Each
session runs, inside the worktree,

```
agy [--conversation <id>] --model <model> <mode flag> [--effort <e>] \
  --add-dir <data dir> --add-dir <state dir> <extra_args> -i "<prompt>"
```

powerqueue writes `<worktree>/.agents/hooks.json` with a `Stop` hook calling
`powerqueue hook --provider gemini` (merged into an existing file) and adds
`.agents/` to the repository's `.git/info/exclude` once. The conversation id
comes from `~/.gemini/antigravity-cli/cache/last_conversations.json`; as a
fallback the daemon also polls the conversation transcript for the DONE /
BLOCKED markers. agy transcripts carry no token counts, so no usage is
recorded for gemini sessions. Set `POWERQUEUE_AGY_HOME` (in `gemini.env` or
the daemon's environment) if agy keeps its state somewhere other than
`~/.gemini/antigravity-cli`.

| Key | Default | Meaning |
|-----|---------|---------|
| `binary` | `"agy"` | Antigravity CLI executable |
| `mode` | `"skip-permissions"` | `skip-permissions` (`--dangerously-skip-permissions`), `accept-edits` or `plan` (`--mode`) |
| `effort` | `"high"` | `--effort` (`low`, `medium`, `high`); unset to leave the default |
| `extra_args` | `[]` | flags appended verbatim |
| `env` | `{}` | environment variables for the session |

### `[budget]` and `[budget.providers.<provider>]`

Budgets are per provider: each of `claude`, `codex` and `gemini` has its own
period, window and model table under `[budget.providers.<provider>]`; the
shared pacing knobs stay in `[budget]`. Older files with the flat keys
(`budget.period_hours`, `[budget.models.fable]`, ...) still load: they are
moved under `budget.providers.claude` on read, `doctor` and `config validate`
note the move, and `config set` rewrites the file to the new shape. Only
Claude is enabled by default.

Shared keys in `[budget]`:

| Key | Default | Meaning |
|-----|---------|---------|
| `provider_order` | `["claude", "codex", "gemini"]` | fallback order between enabled providers |
| `default_model` | `"sonnet"` | model when nothing else applies |
| `low_model` | `"sonnet"` | model for `low` tasks |
| `safety_margin` | `0.05` | fraction of the remaining budget kept unspent (0 to 0.5) |
| `endgame_fraction` | `0.8` | after this fraction of the period, reserved capacity is released |
| `probe_interval_mins` | `15` | how often usage probes run (0 disables them) |

Usage probes ask each enabled provider how much of its allowance is used,
without spending any of it: Codex through `codex app-server`
(`account/rateLimits/read`), Claude through the status line every task's
`settings.json` configures (`powerqueue hook … --event StatusLine`, which
also shows `pq <model> 5h 75% · 7d 89%` in the session), Antigravity through
`agy -p /usage` (experimental). The daemon runs them at start and every
`probe_interval_mins` in the background; `budget probe` runs them now. The
reported weekly reset becomes the period anchor, the reported usage the
calibration, and a provider that reports its allowance exhausted is skipped
until the reset. Details: [docs/budget.md](docs/budget.md#usage-probes-and-observed-anchors).

Per provider (`[budget.providers.claude]`, `.codex`, `.gemini`); keys you
omit keep the provider's defaults:

| Key | claude | codex | gemini | Meaning |
|-----|--------|-------|--------|---------|
| `enabled` | `true` | `false` | `false` | schedule tasks on this provider |
| `period_hours` | `168` | `168` | `168` | usage period length (subscriptions reset weekly) |
| `period_anchor` | none | none | none | RFC 3339 start of a period; set with `budget set-reset --provider <p>` |
| `window_hours` | `5` | `5` | `5` | rolling short window; `0` = no window |
| `period_weighted_tokens` | `80000000` | `60000000` | `40000000` | weighted tokens the period may consume |
| `window_weighted_tokens` | `12000000` | `8000000` | `6000000` | weighted tokens the window may consume |
| `rate_limit_cooldown_mins` | `30` | `30` | `30` | pause the provider after a `rate_limit` error (never past the period end) |

Per model (`[budget.providers.claude.models.fable]`, `.opus`, `.sonnet`,
`.haiku`); a model you mention keeps the shipped values for keys you omit,
models you do not mention stay as shipped (disable one with `enabled = false`):

| Key | fable | opus | sonnet | haiku | Meaning |
|-----|-------|------|--------|-------|---------|
| `rank` | `10` | `20` | `30` | `40` | capability order within the provider (lower = more capable; your own additions default to 50) |
| `share` | `0.25` | `0.35` | `0.35` | `0.05` | fraction of `period_weighted_tokens` |
| `min_criticality` | `critical` | `high` | `low` | `low` | minimum criticality early in the period |
| `relax_after_fraction` | `0.5` | `0.3` | `0.0` | `0.0` | when under-spent models open up one level |
| `weight` | `5.0` | `3.0` | `1.0` | `0.2` | cost weight relative to Sonnet |
| `enabled` | `true` | `true` | `true` | `true` | model may be used |

Shipped Codex models (`[budget.providers.codex.models."gpt-6.1-sol"]`, ...):
`gpt-6.1-sol` (rank 10, share 0.4, critical, weight 2.0), `gpt-6-astra` (20,
0.4, high, 1.5), `gpt-6-luna` (30, 0.2, low, 0.5). Gemini: `gemini-3-pro` (10,
0.7, high, 1.0), `gemini-3-flash` (20, 0.3, low, 0.2). Model names infer their
provider (`fable|opus|sonnet|haiku|claude-*` → claude, `gpt-*|o1*|o3*|o4*|codex*`
→ codex, `gemini-*` → gemini); anything else needs the explicit
`"codex:<name>"` form. Enabled shares must sum to at most 1.0 per provider,
and `default_model` / `low_model` must belong to an enabled provider.

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

[codex]
approval = "on-request"     # also: reasoning_effort, extra_args

[gemini]
mode = "accept-edits"       # also: effort, extra_args
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
- ENG-200: model = opus | gpt-6-astra
- ENG-300: skip

## Models
- critical: fable | gpt-6.1-sol
- high: opus | gpt-6-astra
- normal: sonnet
- low: sonnet
```

The file is re-read whenever it changes. `## Models` and `KEY: model = ...`
are preference lists handed to the budget policy (`|`-separated, most wanted
first; alternatives may belong to other enabled providers); the policy tries
each one through its provider's downgrade chain and falls back to
`budget.provider_order` when none fits. Only `task model` and `add --model`
set a hard override, and even that is downgraded (within its provider) when
the model is out of budget. Full grammar, evaluation order and idioms:
[docs/priority.md](docs/priority.md).

## Budget pacing

Every API call's usage is weighted (`input + 1.25·cache_write + 0.1·cache_read + 5·output`),
multiplied by the model's weight, and charged against its provider's period
budget and rolling window. Each model has a share of the period and a minimum criticality.
Early in the period Fable only serves `critical` tasks; if a tier is under-spent
past `relax_after_fraction`, one level lower qualifies; in the end game two
levels. The predicted cost must also fit the tier's remaining share, the
overall period budget and the window. With several providers enabled, the
task's preferred models are tried in order (each through its own provider's
downgrade chain), then the first eligible model in `budget.provider_order`;
a rate-limited or exhausted provider is skipped until its reset. If nothing
is eligible the task is `throttled` with a retry time.
Usage probes keep the period anchor and calibration current; `budget set-reset`
and `budget set-observed` do it by hand, and `doctor` tells you when to.
Details and worked examples: [docs/budget.md](docs/budget.md).

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
- `powerqueue dashboard --once` prints the dashboard frame as plain text (`--json` for the raw snapshot, with `ledgers.by_provider.<p>`, `cooldowns` and `next_model`); paste it when the live dashboard looks wrong.
- `powerqueue doctor --json` is what to paste into bug reports.

Symptoms and fixes: [docs/troubleshooting.md](docs/troubleshooting.md).
Internals: [docs/architecture.md](docs/architecture.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Agents and humans should also read
[AGENTS.md](AGENTS.md). Security issues: [SECURITY.md](SECURITY.md).

## License

MIT. See [LICENSE](LICENSE).
