# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.3] - 2026-10-05

### Fixed

- Linux resource sampling counted threads as separate processes, multiplying
  RSS (and CPU) by the number of threads. Refresh only real processes and
  exclude thread entries from session tree totals. A 36 MB process with 32
  worker threads previously appeared as 1.19 GB. Historical samples already
  stored in the database are unchanged; new live samples use the corrected sum.

## [0.4.2] - 2026-10-05

### Fixed

- Clear the dashboard and restored shell screen on exit, including `q`, Ctrl+C
  and tmux panes with alternate-screen disabled, without purging scrollback.
- Clear stale `needs_attention` and its reason after prompt submission; permission
  requests also clear on tool completion or newer assistant output. Preserve
  explicit blockers and paused/finished tasks, and ignore historical transcript
  replay as a recovery signal.
- Preserve Claude hook ordering so delayed permission notifications do not
  overwrite subsequent activity; register `PostToolUse` / `PostToolUseFailure`.
- Do not classify courtesy "let me know" closings or requests quoted earlier
  in an answer as blockers.

### Changed

- Verify token accounting against a captured real Claude Code 2.1.289 session
  (text, tool calls and resume), including incremental reads and daemon replay.
- Label weighted token estimates explicitly (`WTOK` in the dashboard) and
  show the raw token breakdown in `task show`.
- Ring the live dashboard's terminal bell on new attention states, show a
  current attention notice and an attach-to-respond hint.

## [0.4.1] - 2026-10-05

### Fixed

- Claude Code sessions started empty: the prompt followed `--allowedTools`,
  which is variadic, so Claude Code read it as more tool rules ("Ignoring
  --allowedTools rule …") and waited for input. The prompt now comes after
  `--`.
- The pane probe, `set-hook` and `new-window` named the tmux session with a
  bare `=name`, a *window* target that tmux resolves against the current
  session's windows first. With the daemon started inside tmux (or that
  session simply the most recently used one), `automatic-rename` calls the
  window running `powerqueue run`/`tune` exactly `powerqueue`, so the daemon
  listed the operator's panes, reported every live session as "tmux pane
  disappeared" and installed its `remain-on-exit` hook on the operator's
  session. Session targets are now `=name:`.
- A crashed session that never wrote a transcript (it died before its first
  prompt) is no longer resumed: `--resume` failed with "No conversation found"
  on every remaining attempt. The retry starts a fresh session and logs
  `session.fresh`.

## [0.4.0] - 2026-10-02

### Added

- `powerqueue tune "<what you expect>"`: describe a change in plain words
  ("ENG-12 should run before ENG-40", "chores are low and use sonnet", "run
  three tasks at once") and let a headless Claude Code session edit drafts of
  `PRIORITY.md` and `config.toml`. The drafts live under
  `<state>/tune/<id>/` next to the originals, a `CONTEXT.md` snapshot of the
  queue, tasks and budget, the full prompt and Claude's answer; the session
  may only edit the drafts and run the read-only `priority check --file`,
  `priority simulate --file --config` and `config validate --file` commands.
  powerqueue validates the result, prints Claude's summary, a unified diff
  per file and the simulated queue with the drafts, and asks before writing
  the live files (`-y` applies directly, `--dry-run` never applies,
  `--scope priority|config|all`, `-m MODEL`, `--timeout SECS`,
  `--no-budget`, `-` reads stdin). `tune --apply [DIR]` applies a kept
  proposal later, `tune --undo` restores the previous files; a running daemon
  is asked to reload. Exit 3 means "proposed but not applied". `--json`
  prints the draft, status, files with diffs and the simulation.
- `[tune]` configuration: `model` (default `sonnet`), `timeout_secs` (600),
  `extra_args`, `keep_drafts` (20; finished drafts beyond that are pruned).
- `priority check --file PATH` validates a draft rules file;
  `priority simulate --config PATH` tries a draft `config.toml` (budget,
  concurrency) alongside `--file`; `config validate --file PATH` validates a
  draft config.
- `doctor` reports tune drafts that were proposed but never applied, or runs
  that failed or produced invalid files, with the directory to look at.
- Events `tune.applied` / `tune.undone` in the timeline (`logs --events`).

### Changed

- A timed-out headless session is killed together with its process group,
  so tool commands it started cannot keep the output pipes open.

## [0.3.0] - 2026-10-02

### Added

- `priority simulate [--file PATH] [--linear] [-a] [--reasons] [--no-budget] [-n N]`:
  dry-run the rules against every open task, rank them the way the scheduler
  does, and show what the budget policy would run for each, without writing
  anything. `--file` tries a draft before it replaces the live file.
- `powerqueue update [--check] [--version TAG] [-y] [--force]` self-updates
  from GitHub releases: downloads the platform tarball and its `.sha256`,
  verifies the checksum, stages the new binary next to the current one, runs
  it with `--version`, then renames it over the old file atomically (a
  running daemon keeps the old version until `stop` / `run`). `--check` exits
  10 when a newer release exists; `--json` prints `current`, `latest`,
  `updated` and `path`. `GITHUB_TOKEN`, `POWERQUEUE_UPDATE_API` and
  `POWERQUEUE_UPDATE_TARGET` are honoured.
- `powerqueue reset`: start over after a bad run. Stops the daemon, kills the
  task windows and the tmux session, removes task worktrees and stray
  directories under the worktree root (dirty or unpushed worktrees are kept
  and reported unless `--force`), deletes the per-task state directories and
  empties the database while keeping config, secrets, `PRIORITY.md` and logs.
  `--dry-run` prints the plan (`--json` supported), `-y` skips the prompt,
  `--delete-branches` also deletes the local `pq/*` branches, `--everything`
  also clears the `kv` table (budget calibration, cooldowns, probe results),
  `--revert-linear` moves the open tasks' Linear issues back to the first
  `linear.queued_states` entry. `Store::reset` backs it.
- Linear queue scoping: `linear.cycle` (`any`, `active`/`current`, `next`,
  `active-or-next`, `none`) and `linear.projects` (project names) filter
  issues server-side. Issues carry their cycle into tasks (database schema
  v3: `tasks.cycle`, `tasks.cycle_number`), `task show` and `linear sync`
  print it, and `PRIORITY.md` conditions can use `cycle`
  (`active|next|past|future`) and `cycle_number` (`>`/`<` supported), e.g.
  `+150 if cycle: active`.
- `linear.manage_states = false` stops the daemon from moving issues between
  workflow states (for users whose own Claude skills or CI own the status);
  comments still follow `post_comments`. An empty `in_progress_state`,
  `done_state` or `blocked_state` now means "no state change" for that
  transition.
- Customisable task prompt: `[prompt] template` points to a Markdown file
  with `{{placeholders}}` (`key`, `title`, `description`, `url`, `labels`,
  `cycle`, `branch`, `working_rules`, `completion_protocol`,
  `attempt_notes`, `default_prompt`, ...); `[prompt] instructions` appends
  a `## Instructions` section to every prompt; `.powerqueue.toml` can set
  `prompt_template` per repository. A missing or unreadable template falls
  back to the built-in prompt with a `prompt.template_error` event (also
  raised once per unknown placeholder), `doctor` checks the template, and
  `powerqueue task prompt <task>` prints the prompt a task would receive
  (`--json` includes the template path and warnings). An example template
  that reproduces the built-in prompt ships in
  `docs/examples/prompt-template.md`.

### Fixed

- `dashboard` clears the terminal when it starts, after a resize and when it
  comes back from `attach`, so the previous shell contents no longer show
  through the first frame.

## [0.2.0] - 2026-10-02

### Added

- Cleanup commits changes a session left uncommitted
  (`cleanup.commit_uncommitted`, default on; event `cleanup.autocommit`)
  before pushing or removing the worktree, so sandboxed or interrupted
  sessions never lose work. Sandboxed Codex sessions are told not to commit
  (Codex keeps `.git` read-only in every sandboxed mode; only `yolo` commits).
- `task retry` on a task whose session is still alive ends that session first
  (`session.ended`), so the retry actually relaunches instead of waiting on a
  slot the old session kept.
- Config: per-provider budgets (`[budget.providers.<p>]` for `claude`, `codex`
  and `gemini`, each with its own period, window and model table; shared
  knobs stay in `[budget]`); old keys still load (`budget.period_hours`,
  `[budget.models.<tier>]` are moved under `budget.providers.claude` on read,
  `doctor` / `config validate` note it and `config set` rewrites the file).
  Models carry a `rank` (capability order) and any model name can be added in
  config; `[codex]` and `[gemini]` sections hold the launch settings of the
  Codex CLI and (experimental) Antigravity CLI providers.
- `budget set-reset`, `budget set-observed` and `budget clear-limits` take
  `--provider <claude|codex|gemini>` (default `claude`); `hook` takes
  `--provider` too. `task model` and `add --model` accept any provider's model.
- Database schema v2: `sessions.agent_session_id` for CLIs that generate
  their own ids; the Claude calibration moved to kv `budget.calibration.claude`.
- Multi-provider scheduling: the budget policy considers every enabled model
  of every enabled provider (in `budget.provider_order`), tries the task's
  preference list in order (each model through its provider's downgrade
  chain), then falls back to the first eligible model in provider order. A
  `task model` override never crosses providers. Throttled tasks retry at the
  earliest reset across providers.
- Usage probes: Codex (`codex app-server` → `account/rateLimits/read`), Claude
  (the per-task `statusLine`, which also shows `pq <model> 5h 75% · 7d 89%`
  in the session) and Antigravity (`agy -p /usage`, experimental). The daemon
  runs them at start and every `budget.probe_interval_mins` in the
  background; `budget probe [--provider P]` runs them now. Observed usage
  calibrates the ledger, the reported weekly reset becomes the period anchor,
  and a provider that reports its allowance exhausted is skipped until the
  reset. Results are logged as `budget.probe` events.
- `budget show` prints one section per enabled provider with the observed
  usage, its age and the anchor source; `budget show --json` is now
  `{"providers": {"claude": {...}}, "next": {...}}`.
- Rate limits cool down the provider of the session's model (its own
  `rate_limit_cooldown_mins`, capped at its own period end); other providers
  keep working. A Claude pane showing `Usage limit reached · continuing
  automatically` is treated as waiting (`session.waiting_for_reset`), not as
  hung, while the provider is on cooldown.
- `run` lists the enabled providers and the probe interval at start; `hook`
  routes `--provider` payloads through the provider's normaliser and accepts
  the payload as a trailing argument (Codex `notify`).
- Codex CLI sessions: tasks on a Codex model run `codex -C <worktree> -m
  <model> ...` with completion through `notify` (`powerqueue hook --provider
  codex`), trust seeded per session, the data/state dirs writable through
  `--add-dir`, the thread id discovered from the rollout, usage from
  `token_count`, rate-limit errors in the rollout putting Codex on cooldown,
  and crash restarts via `codex resume <thread>`.
- Antigravity CLI (`agy`) sessions, experimental and unverified: `agy
  --model ... -i "<prompt>"` with a `Stop` hook in `<worktree>/.agents/hooks.json`
  (excluded from git), conversation discovery from `last_conversations.json`
  and completion-marker polling of its transcript.
- `powerqueue hook` accepts the payload as a trailing argument (Codex
  `notify`) and normalises other providers' payloads to the Claude shape.
- Session code split into `session/{agent,claude,codex,gemini}.rs`.
- `PRIORITY.md` model lists: `## Models` entries and `KEY: model = ...`
  overrides take `|`-separated alternatives in preference order
  (`critical: fable | gpt-6.1-sol`), which may belong to different providers.
  Unknown names fail with the line number and the alias rules; repeats warn.
  `priority check`, `priority explain` and `task explain` print the lists
  (`Evaluation.models`; `Evaluation.model` stays the first entry).
- Dashboard: the budget panel shows one block per enabled provider (a header
  with period, elapsed, window and any cooldown, then a gauge per model) and
  the header line carries a compact per-provider summary (`cl 34/12%  cx
  17/–`) plus `next <model>`, what the policy would run now. `dashboard
  --once --json` adds `ledgers`, `provider_order`, `cooldowns` and
  `next_model` (`ledger` stays Claude's for older readers).
- `doctor` checks every enabled provider: `<p>` (binary and version),
  `<p> auth` (logged in), `<p> models` (shares ≤ 1, at least one enabled
  model), `<p> budget anchor` (config / observed by a probe / Monday default,
  with the matching hint), `<p> usage probe` (no probe yet, failing, or older
  than 3 × `probe_interval_mins`), `<model> reservation` (pacing of the
  provider's most capable model) and `<p> window pressure`; `gemini` gets an
  "experimental" warning. Claude's check names are unchanged.
- `init` asks "Also run tasks on Codex CLI / Antigravity CLI?" when the binary
  is on PATH (warning when it is not logged in); `init --provider codex
  --provider gemini` answers non-interactively, `init --reconfigure` offers
  the same toggles, and the summary lists the enabled providers.
- `task model` / `add --model` warn when the model's provider is disabled in
  config; `status`, `task show` and `task explain` print non-Claude models
  with their provider (`gpt-6-astra (codex)`) and `--json` includes
  `provider`.
- README "Providers" section, the landing page's "Three subscriptions, one
  queue" card, and troubleshooting notes for the new doctor checks.

- `dashboard --once` renders one frame as plain text and exits (works without
  a TTY; `--json` prints the snapshot), and `dashboard --ascii` draws with
  `*`/`>`/`#` and `+-|` borders (automatic when the locale is not UTF-8).
- `doctor` warns when `TERM` is unset/`dumb` or the locale is not UTF-8.
- `init` offers every permission mode (`acceptEdits`, `auto`,
  `bypassPermissions`, `dontAsk`, `plan`, `default`) with a one-line
  description each, and `--permission-mode MODE` sets it without the prompt
  (validated in non-interactive mode).
- `init` on an existing install offers "Change settings"; `init --reconfigure`
  walks the editable settings (repository, default branch, Linear team and
  states, concurrency, permission mode, weekly budget, reset anchor) with the
  current values as defaults and writes only those keys back.
- `config get <key>`, `config set <key> <value>` and `config unset <key>`
  change single keys by dotted path. Edits keep comments and formatting
  (`toml_edit`), are validated before anything is written, and ask a running
  daemon to reload.

### Changed

- `budget show --json` no longer prints Claude's ledger at the top level;
  read it from `.providers.claude` (`.next` holds the policy's answer for a
  normal task).
- `dashboard` refuses to start without `config.toml` (same "run `powerqueue
  init`" message as `status`), or when stdin/stdout are not a terminal or
  `TERM` is unset/`dumb` (exit 2 with a plain error) instead of writing escape
  sequences into a pipe; terminal initialisation errors are reported with
  context.
- Attaching from the dashboard inside the powerqueue tmux server uses
  `switch-client` and keeps the dashboard running; nested attaches from
  another tmux server clear `$TMUX` so tmux accepts them.
- Budget gauges no longer hard-code a black background.

## [0.1.0] - 2026-10-01

### Added

- Project documentation: README, priority grammar, budget algorithm,
  troubleshooting, architecture.
- CI (fmt, clippy, test, doc, audit) and release workflows; Dependabot;
  issue and pull request templates.
- `justfile` with build, test, lint, doc, ci and dev-state recipes.
- End-to-end tests driving the real daemon with a fake Claude Code binary
  (`tests/fixtures/fake-claude.sh`) on a private tmux server.
- Workspace trust pre-seeding (`claude.trust_workspace`) and an always-on
  `Bash(powerqueue task *)` allow rule so unattended sessions never stall on
  the trust dialog or on the completion command.
- Account-wide rate-limit cooldowns: a `rate_limit` reported by one tier pauses
  every tier until the cooldown or the period reset.

- Linear integration: poll queued issues per team, state and label; move
  issues through workflow states; post progress comments.
- Manual tasks with `powerqueue add`.
- `PRIORITY.md` rules: criticality sections, scoring, per-ticket overrides,
  model mapping, optional Jev scoring; live reload.
- One git worktree and one tmux window per task; interactive Claude Code
  sessions with hooks reporting back through `powerqueue hook`.
- Crash recovery with backoff and `--resume` of the same session id.
- Token usage learning from Claude Code transcripts; CPU/RSS sampling.
- Budget pacing across the subscription period and rolling window, with
  per-tier shares, minimum criticality, relaxation, end game, safety margin,
  rate-limit cooldowns, calibration (`budget set-reset`, `budget set-observed`)
  and a cost estimator.
- Live dashboard (`powerqueue dashboard`), one-shot `status`, `task show`
  timelines, `task explain`.
- `powerqueue doctor` diagnostics with fix hints and `--fix`.
- Secrets in the OS keychain with a 0600 file fallback and
  `POWERQUEUE_SECRETS=file`.
- XDG paths with `POWERQUEUE_HOME` override; rotating JSON logs.

[Unreleased]: https://github.com/aleandros/powerqueue/compare/v0.4.3...HEAD
[0.4.3]: https://github.com/aleandros/powerqueue/releases/tag/v0.4.3
[0.4.2]: https://github.com/aleandros/powerqueue/releases/tag/v0.4.2
[0.4.1]: https://github.com/aleandros/powerqueue/releases/tag/v0.4.1
[0.4.0]: https://github.com/aleandros/powerqueue/releases/tag/v0.4.0
[0.3.0]: https://github.com/aleandros/powerqueue/releases/tag/v0.3.0
[0.2.0]: https://github.com/aleandros/powerqueue/releases/tag/v0.2.0
[0.1.0]: https://github.com/aleandros/powerqueue/releases/tag/v0.1.0
