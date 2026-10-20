# Command reference

[Documentation](README.md) · [Project overview](../README.md)

Use `powerqueue --help` or `powerqueue <command> --help` for the flags
supported by your installed version.

Global flags work on every command.

| Flag | Meaning |
|------|---------|
| `-v`, `-vv` | debug / trace logging on stderr |
| `-q`, `--quiet` | only print errors |
| `--home DIR` | base directory for config/data/state (env `POWERQUEUE_HOME`) |
| `--json` | machine-readable output where supported (status, add, task, project, priority, tune, budget, linear, github, doctor, update, service, config, `logs --events`) |
| `--no-color` | disable colours (env `NO_COLOR`) |

## Setup and daemon

| Command | What it does |
|---------|--------------|
| `init [--repo PATH] [--team KEY]... [--linear-key K] [--jev-key K] [--no-linear] [--permission-mode MODE] [--provider P]... [--non-interactive] [--reconfigure] [--force]` | guided first-time setup; writes `config.toml` and `priority.toml`, stores keys; `--no-linear` sets `linear.enabled = false` (GitHub can be enabled separately); `--permission-mode` picks `acceptEdits` (default), `auto`, `bypassPermissions`, `dontAsk`, `plan` or `default`; when `codex` / `agy` are on PATH it asks whether to run tasks on them too (`--provider codex --provider gemini` answers yes non-interactively and warns when the CLI is not logged in); on an existing install the menu offers "Change settings", and `--reconfigure` walks the editable settings (repository, default branch, Linear team and states, concurrency, permission mode, weekly budget, reset anchor, providers) with the current values as defaults, keeping keys and every other key |
| `project register ID PATH [--github OWNER/REPO] [--token-env NAME] [--trusted-approver LOGIN]...` | register/reconfigure a checkout and optional GitHub intake in one validated edit; creates a GitHub/manual config if absent, retains unspecified settings, rejects source retargeting and pinned-checkout moves; token/approver flags require `--github` |
| `project list` | show effective checkout/worktree paths and source mappings; `--json` also includes per-project settings and shared limits |
| `run [--once] [--offline]` | run the scheduler in the foreground; `--once` does one admission pass and supervises its background setups through launch/failure before exiting; `--offline` skips Linear and GitHub |
| `stop` | ask the running daemon to exit (sessions keep running in tmux) |
| `pause [--reason TEXT]` | stop launching sessions: running ones continue, the daemon keeps monitoring, cleaning up and syncing, nothing new starts (crashed sessions are not relaunched either) until `resume`; survives a daemon restart; `status`, `dashboard` and `doctor` show it |
| `resume` | launch sessions again after `pause` |
| `reset [-y] [--dry-run] [--force] [--delete-branches] [--everything] [--revert-linear]` | start over: stops the daemon, kills the tmux session, removes task worktrees (dirty or unpushed ones are kept and reported unless `--force`) and stray directories under the worktree root, empties `state/tasks/` and every table of the database; `--dry-run` prints the plan (`--json` for machine-readable), `-y` skips the prompt (required without a TTY), `--delete-branches` also deletes the local `pq/*` branches, `--everything` also clears `kv` (usage readings, cooldowns, probe results, pause), `--revert-linear` moves open Linear issues back to the first `linear.queued_states` entry. Config, secrets, `priority.toml` and logs are never touched |
| `dashboard [--once] [--ascii] [--project ID]` (`ui`, `top`) | live TUI: task table, one budget block per enabled provider (period, window, cooldown, a gauge per model), a header with the compact per-provider summary (`cl 34/12%  cx 17/–` = period/window spent) and `next <model>` (what the policy would run now); needs an interactive terminal (exit 2 otherwise). `--once` prints one frame as text and exits (works in pipes; with `--json` prints the snapshot); `--ascii` uses `*`/`>`/`#` and `+-\|` borders (automatic when the locale is not UTF-8) |
| `status [-a] [--project ID]` (`ls`) | one-shot table with execution project; `-a` includes completed/failed/cancelled; `--project` filters tasks (counts remain global) |
| `doctor [--fix] [--offline]` | diagnostics and tuning advice, per enabled provider (binary and version, logged in, model shares, period anchor source, probe freshness, top-model pacing, window pressure, the learned usage rate vs the configured budget, whether the top model's share can hold one typical task, a daemon-wide pause); `--fix` applies safe repairs |
| `update [--check] [--version TAG] [-y] [--force]` | replace this binary with a [GitHub release](https://github.com/aleandros/powerqueue/releases): downloads `powerqueue-<target>.tar.gz` and its `.sha256`, verifies the checksum, writes the new file next to the current one, runs it with `--version`, then renames it over the old one (atomic; a running daemon keeps the old version until `service restart`, or `stop` / `run`); asks before replacing unless `-y` / `--yes` or stdin is not a terminal; `--check` only reports (exit 0 up to date, 10 newer release available); `--version v0.4.0` installs a specific tag; `--force` reinstalls the same version; `--json` prints `{"current","latest","updated","path",...}`; `GITHUB_TOKEN` is used for the API when set; `POWERQUEUE_UPDATE_API` / `POWERQUEUE_UPDATE_TARGET` override the API base and target triple (mirrors, tests) |
| `service install [--print] [--manager systemd\|launchd] [--no-start] [-f] [--linger] [--env KEY=VALUE]...` | run the daemon as a user service: writes the systemd user unit (`~/.config/systemd/user/powerqueue.service`) or launchd agent (`~/Library/LaunchAgents/dev.powerqueue.plist`) for `<this binary> run` with the current `PATH` (and `POWERQUEUE_HOME`, `POWERQUEUE_SECRETS`, `XDG_*_HOME`, `TMUX_TMPDIR`, `LANG`, `LC_ALL` when set), enables it and starts it. Restarts after crashes, not after `stop`, and leaves tmux sessions running when it stops (`KillMode=process` / `AbandonProcessGroup`). `--print` only prints the file (`--manager` picks the platform); `--no-start` enables without starting; `-f` / `--force` replaces a different existing file (shown as a diff otherwise) and restarts a running service with it; `--linger` runs `loginctl enable-linger` so the service outlives your login (Linux); `--env` adds variables (stored in plain text, so no API keys); needs `init` first; `--json` reports what was done |
| `service uninstall [-f]` | stop and disable the service and remove its file; tmux sessions keep running |
| `service start` / `stop [-f]` / `restart [-f]` | control the service (`systemctl --user` / `launchctl`). `stop` and `restart` refuse a unit that would kill the tmux sessions until it is regenerated, or with `-f` |
| `service status` | unit path (generated or hand-written), binary, enabled, running (pid), linger (Linux), whether sessions survive a stop, and problems: a missing or different binary, tools its `PATH` cannot find, a daemon running outside the service; exits 0 when the service is running and 3 otherwise; `--json` |
| `service logs [-f] [-n N]` | the service manager's output: `journalctl --user -u powerqueue.service`, or the launchd files `<state>/logs/launchd.{out,err}.log`; the daemon's own log is `logs` |
| `logs [-f] [-n N] [-t TASK] [-l LEVEL] [--events]` | read the daemon log (default 200 lines); `--events` shows the DB timeline instead; `-f` follows either |
| `completions <shell>` | shell completions (bash, elvish, fish, powershell, zsh) |

Project filters on `status`, `task list` and `dashboard` affect only the task list; shared budgets and slot/count summaries remain global. See the [multi-project walkthrough](multi-project.md).

## Tasks

| Command | What it does |
|---------|--------------|
| `add "title" [-d DESC\|-] [-c CRIT] [-m MODEL] [-k KEY] [-l LABEL]... [--paused] [--project ID]` | enqueue a manual task in a registered execution project; omitted `--project` always selects `default`, unknown IDs fail; `-d -` reads stdin |
| `task defer <task> <until> [--reason TEXT]` | set or replace a local launch hold; `<until>` is an RFC 3339 timestamp with explicit offset or a positive duration such as `2h` |
| `task defer <task> --clear` | clear only the local hold; source deferrals and other scheduling restrictions still apply |
| `task analyze <task> [--show]` | explicitly request advisory classification, registered-project recommendation and preliminary plan; `--show` reads saved advice without a model call. Requires opt-in `[analysis]`; never approves or assigns work. Exit 3 means unresolved/stale. See [issue analysis](issue-analysis.md) |
| `task project <task> <id>` | reassign an unstarted task to a registered execution project; refuses tasks with previous execution history; validates again when the daemon applies the command |
| `task show <task>` | details, execution project/configured checkout, saved/current routing decisions and timeline |
| `task list [-a] [--project ID]` | same as `status` |
| `task complete <task> [-s SUMMARY] [--pr URL]` | mark completed (Claude calls this from inside the session); with `--pr` hand it off for review instead: `in_review`, slot and worktree released, the daemon watches the PR (see [Review](task-lifecycle.md#review-pull-requests)). Refused while the task is `starting` (the daemon is launching it; try again once it is running) |
| `task block <task> [-r REASON]` | mark blocked / needs a human; refused while the task is `starting` |
| `task cancel <task>` | cancel and release resources |
| `task pause <task>` | do not schedule; a running session stops after its turn |
| `task resume <task>` | resume a paused or needs-attention task; one the PR watcher parked goes back to `in_review` |
| `task retry <task>` | re-queue a failed, cancelled or completed task (also crashed, throttled, paused, needs-attention); one parked with its review rounds used up runs one more round; one `in_review` resumes its session for a review round now; in any other state the daemon ignores it (`task.retry_ignored`, debug) |
| `task explain <task>` | execution project/configured checkout, saved/current routing decisions, current score and model decision, with reasons; for Linear tasks also what it waits on (pending `blocked by` issues, open sub-issues of a parent) |
| `task model <task> <model\|auto>` | force (or clear) the model for the next attempt (`fable`, `opus`, `sonnet`, `haiku`, or another provider's model such as `gpt-6.1-sol`, `gemini-3-pro`, `codex:<name>`); the policy may still downgrade it within the same provider when the model is out of budget |
| `task prompt <task>` | print the prompt the next attempt would receive (renders `prompt.template` with attempt = attempts + 1, the task's last error and its forced/last model); nothing is launched. `--json` prints `{"task", "template", "prompt", "warnings"}` |
| `task output <task> [-n LINES]` | last screen of the task's tmux pane (default 60 lines) |
| `task send <task> "message"` | type a message into the running session |
| `attach [<task>] [--print]` | open the task's tmux window; no task = the powerqueue session |

`<task>` is a key (`ENG-123`, `manual-1a2b3c4d`), a full id, or an id prefix.
`pause`, `resume`, `cancel` and `retry` are queued for the daemon; with no
daemon running (no heartbeat for 30 s) they are applied directly when the
state machine allows it. Agent sessions can survive the daemon: offline
`resume` returns a task with a recorded live session to `running`, and
`cancel`/`retry` remain queued until the daemon can stop that session. Restart
`powerqueue run` to process those queued controls. `complete` and `block` always
write directly.

## Deferring a task

```sh
powerqueue task defer ENG-123 2h --reason "Wait for deployment"
powerqueue task defer 'owner/repo#43' 2026-10-20T09:00:00-06:00 --reason "Maintenance window"
powerqueue task show ENG-123
powerqueue task explain ENG-123
powerqueue task defer ENG-123 --clear
```

Running `defer` again replaces the local time and reason. Without `--reason`, the
previous reason is cleared. Durations are measured once, when the command runs;
`2h` means two elapsed hours. Absolute timestamps require `Z` (UTC) or a numeric
UTC offset; there is no machine-local timezone default or daylight-saving guess.
Past absolute timestamps are allowed and immediately eligible under this gate.

To set a time in **either Linear or GitHub**, put these standalone lines in the
issue description/body (not a comment or label):

```text
Powerqueue-Defer-Until: 2026-10-20T09:00:00-06:00
Powerqueue-Defer-Reason: Wait for the maintenance window
```

The reason is optional. This example means October 20 at 09:00 at UTC−06:00,
equivalently 15:00 UTC. Use the offset for the intended date. Source descriptions
accept absolute timestamps only: a relative `2h` would otherwise slide on every
poll. Change the line to reschedule; remove **both** lines to clear the source
hold. Duplicate directives, invalid timestamps, or a reason without a time hold
the task until corrected. `doctor` explains the error. Directives are reserved
standalone lines, even inside a code fence; keep examples quoted or indented with
non-whitespace text if they should not act as metadata.

Changes take effect after the normal source sync (or `linear sync --apply` / `github sync --apply`);
normal source intake, routing and approval requirements still apply. GitHub body
edits may require renewed approval under the configured admission policy. Local
CLI holds and source holds are independent; **both must expire**. CLI `--clear`
does not edit or override the issue. Manual-task descriptions do not interpret
these directives.

A deferred task keeps its workflow state and remains visible in `status`, the
dashboard, and status JSON output. Dashboard rows show `[deferred]`; select a row for
the underlying state, source association, eligibility time and reason. `task show`,
`task explain`, and `doctor` show local and source holds separately, including
expired ones. Expiry is an earliest eligibility time, **not a guaranteed start**:
priority, dependencies, user/rule pauses, queue capacity, budget, routing,
admission and provider cooldowns still apply.

`defer` writes immediately and works without a daemon. It never stops a running
session. It holds future retries and review-round launches; `resume`, `retry`,
crash recovery and budget resets preserve it. It can also be set on a finished
task in preparation for a later retry. A launch already claimed as `starting`
refuses local deferral edits; retry after it becomes running. Source edits received
after a launch claim apply to subsequent launches. Unobserved remote edits cannot
retroactively undo a launch. Restart preserves the absolute times and reasons.
Local edits and their audit events commit together. `task.deferral_changed` and
`task.source_deferral_changed` record changes; expiry needs no reset or state change.

## Priority rules

| Command | What it does |
|---------|--------------|
| `priority show` | print the parsed rules (model preference lists joined with ` \| `) |
| `priority show/check/edit/path [--project ID]` | inspect, validate, edit or locate the typed TOML policy; defaults to machine policy; `check` also accepts `--file PATH` |
| `priority migrate --from MARKDOWN --to TOML` | explicitly convert legacy rules, report warnings, and create a new TOML file; never overwrites files or updates configuration |
| `priority explain TASK` | explain evaluation using the task's execution-project policy |
| `priority simulate [--project ID] [--file PATH] [--config PATH] [--all] [--linear] [--reasons] [--no-budget] [-n LIMIT]` | dry-run ranking and budget decisions; live policies resolve per task, `--file` applies a draft; no state changes |
| `tune "what you expect" [--project ID] [--intent PATH] [--scope priority\|config\|all] [-m MODEL] [-y] [--dry-run] [--no-budget] [--timeout SECS]` | describe the change in plain words ("ENG-12 should run before ENG-40", "chores are low and use sonnet", "run three tasks at once") and let a headless Claude Code session edit **drafts** of `priority.toml` and `config.toml` under `<state>/tune/<id>/`; powerqueue validates the result, prints Claude's summary, the diff and the simulated queue with the drafts, then asks before writing the live files (`-y` applies directly, `--dry-run` never applies). Exit 0 applied/unchanged/dry run, 1 failed/invalid/declined, 3 proposed but not applied (no terminal, no `-y`). `-` reads the instruction from stdin |
| `tune --apply [DIR] [--project ID]` | apply the newest proposed draft (or `DIR`) after reviewing it; `-y` skips the question |
| `tune --undo [--project ID]` | restore the files the most recent apply replaced; refuses legacy drafts, missing snapshots or subsequent live edits before changing any file (a running daemon is asked to reload) |

`tune` reads free-form `PRIORITY.md` beside the selected policy (or `--intent PATH`)
and snapshots it with the current executable TOML. `--project ID` selects the
project's effective policy and queue; omitted means the machine policy. The
proposal records its project, so later `--apply DIR` uses the same target.
`--project` also filters the newest proposal/apply/undo selection. Configuration
edits still target machine `config.toml`; repository overrides remain read-only.
Out-of-scope edits are ignored in both application and simulation. Machine-policy
proposals simulate other projects using their own policies.

Each proposal reports whether intent differs from the last applied proposal;
`doctor` reports changed intent and repository proposals awaiting a commit.
Changing prose alone never changes scheduling. Missing default intent is allowed;
an explicit unreadable/missing `--intent` fails. Intent is a read-only snapshot:
changing it or live policy after drafting requires a new proposal.

Repository policies work with the same approval options. For `default-branch`
policies, tuning starts from the configured committed copy and requires the
working-copy policy to match it. Apply writes a reviewable working-copy change;
review, commit and merge it into the configured branch before the daemon sees
it. No branch, commit or push is performed by tune. Undo restores that working
copy too; reverting an already merged policy requires the same commit workflow.
Applied drafts are retained for undo and intent comparison.
Drafts from before the TOML migration must be restored manually from their
`original/` directory. Undo checks all affected files before restoring any, so
newer config edits cannot cause a partial policy rollback. `tune` and
`priority edit` require valid repository overrides; fix `.powerqueue.toml`
before using either command. A missing TOML policy beside ordinary Markdown
intent is allowed; executable legacy Markdown directives still require migration.

## Budget

| Command | What it does |
|---------|--------------|
| `budget show` | per enabled provider: how much of the period and window is used (from the provider's own reading when there is one), spend per model, the learned exchange rate between weighted tokens and the provider's percentages, observed usage and its age, anchor source and cooldowns; then what the policy would run now for a typical task per criticality and for every task waiting in the queue (`--json`: `{"providers": {...}, "next": {...}, "queued": [...]}`) |
| `budget probe [--provider P]` | run usage probes (Codex `app-server`, the last stored Claude status-line reading, `agy /usage`) and store the results; exits 1 when a probe fails |
| `budget set-reset <when> [--provider P]` | record a provider's period reset instant (from `/usage` in Claude Code; RFC 3339 or `in 3d4h`); default provider `claude` |
| `budget set-observed <percent> [--provider P]` | record the period percentage a provider shows (e.g. `43%` from `/usage`) as a reading: pacing starts from it, and two readings far enough apart teach the exchange rate (see [docs/budget.md](budget.md#learning-the-rate)) |
| `budget clear-limits [--provider P]` | forget a provider's rate-limit cooldowns |
| `budget estimate [<task>]` | the cost estimator's view of a task (or all history) |

`--provider` accepts `claude`, `codex` or `gemini` (`antigravity`/`agy` are aliases of `gemini`).

## Linear and GitHub Issues

| Command | What it does |
|---------|--------------|
| `linear teams` | teams visible to the API key |
| `linear states <team>` | workflow states of a team |
| `linear test` | verify the key (prints the viewer) |
| `linear sync [--apply]` | fetch queued issues and print what would change; `--apply` applies it |
| `github test` | check token access to each configured GitHub source; report per-source errors with a nonzero exit |
| `github sync [--apply]` | preview all GitHub sources with their execution projects, approval decisions/revocations, metadata updates and confirmed closures; `--apply` imports successful sources even if another fails; per-source errors cause a nonzero exit; `--json` includes `sources` and `errors` |

## Config and secrets

| Command | What it does |
|---------|--------------|
| `config show` | effective configuration as TOML, with every key the repo's `.powerqueue.toml` set marked `# .powerqueue.toml` and a header naming the file and the branch it was read from (`working tree` or `origin/main`, see `repo.overrides_from`); `--json` adds `repo_overrides` (`file`, `source`, `rev`, `keys`) |
| `config path` | config/data/state paths, the rules file and the repo's `.powerqueue.toml` |
| `config get <key>` | one value by dotted key (`claude.permission_mode`, `budget.providers.claude.models.fable.share`, `linear.team_keys`) as TOML; `--json` for JSON |
| `config set <key> <value>` | change one key; `<value>` is TOML (`3`, `true`, `["ENG","OPS"]`) or a bare string (`auto`); the result is validated before anything is written, comments in `config.toml` survive, and a running daemon is asked to reload (new sessions use the new value) |
| `config unset <key>` | remove a key so its default applies again |
| `config edit` | open `config.toml` in `$EDITOR` |
| `config validate [--file PATH]` | validate `config.toml` (or a draft) with the repo's `.powerqueue.toml` applied, so a bad value the repo sets is reported too; says which keys the repo sets and from which branch |
| `secrets set <linear\|github\|jev\|analysis> [value]` | store a key (prompts if omitted) |
| `secrets unset <name>` | remove a key |
| `secrets list` | which keys are configured and where they come from |
| `hook --task ID [--session SID] --event EVENT [--provider P] [PAYLOAD]` | internal; called by agent CLI hooks (hidden); `--event StatusLine` is the Claude status line (stores rate limits, prints a short status); `PAYLOAD` replaces stdin (Codex `notify`) |

Routing previews are included in `github sync` and `linear sync` (dry run by
default, `--apply` persists). Conflicting matches and unmatched work with no
source fallback are held with reasons. `task show`/`task explain --json` expose
saved and current routing; see [routing configuration](configuration.md#routing).
`task project` reassigns only unstarted work, and routing must agree with the new
assignment before it can launch.

## Triage completion

`task complete <task> --summary-file PATH --outcome OUTCOME` records a full UTF-8
Markdown analysis and recommended next action for a `leave-ticket-open` workflow.
`--summary-file` conflicts with `--summary`; `--outcome` conflicts with `--pr`.
Outcomes: `needs-information`, `needs-decision`, `ready-for-agent`, `manual-action`,
`no-action-recommended`. Linear report/label delivery is retried by the daemon
without holding a session slot or closing the source issue. Use `task retry`
for an explicit retriage; recommendation labels do not admit coding work.
See [triage workflows](triage.md) for configuration and application rollout.
