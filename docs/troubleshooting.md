# Troubleshooting

Start with `powerqueue doctor`. It checks the environment, configuration,
secrets, database state and the scheduling algorithm, and prints a fix hint for
every problem. `doctor --fix` applies the safe repairs (`src/doctor.rs`):
tasks that look live without a live session are marked `crashed` so the
daemon relaunches them; orphan worktrees that are clean and pushed are
removed; dead orphan tmux windows are closed; a stale `daemon.lock`/`daemon.pid`
pair with no live process is deleted. Nothing else is changed automatically.

Each entry below is symptom → checks → fix.

## The daemon will not start

**Symptom**: `powerqueue run` exits with
`another daemon is running (pid N); stop it with powerqueue stop`.

Checks:

```sh
powerqueue config path                       # find the state dir
cat ~/.local/state/powerqueue/daemon.pid     # pid of the last daemon
ps -p "$(cat ~/.local/state/powerqueue/daemon.pid)"
powerqueue doctor                            # "daemon" and "daemon lock" checks
```

The daemon takes an exclusive advisory lock on `<state>/daemon.lock` and
writes its pid to `daemon.pid`. Only one daemon per state directory can run.
The lock is released when the process dies, so this error normally means a
daemon really is alive.

Fixes:

- Another daemon is alive: use it (`powerqueue dashboard`) or stop it with
  `powerqueue stop`.
- The pid is dead but the files remain: harmless for `run`, but
  `powerqueue doctor --fix` removes the stale `daemon.lock` and `daemon.pid`
  (or delete them by hand).
- You want two instances: give each its own `POWERQUEUE_HOME`.

**Symptom**: `cannot read .../config.toml (run powerqueue init first)`.

Run `powerqueue init`, or point `--home`/`POWERQUEUE_HOME` at the right place.

## No tasks are picked up

**Symptom**: the daemon runs, `status` is empty.

Checks:

```sh
powerqueue linear test                 # key works? prints the viewer
powerqueue linear states ENG           # exact state names
powerqueue linear sync                 # what a sync would do (dry run)
powerqueue config show | sed -n '/\[linear\]/,/^\[/p'
```

Fixes:

- `linear.queued_states` must contain the exact workflow state *name*
  (default `["Todo"]`). Names differ per team; copy them from `linear states`.
- `linear.team_keys` empty means every team the key can see; a wrong key
  matches nothing.
- `linear.assignee = "me"` restricts to issues assigned to the key's user.
- `linear.required_labels` must match at least one label on the issue;
  `linear.excluded_labels` (default `no-agent`) removes issues.
- Issues already known as `completed`/`cancelled` tasks are not re-created.
  Use `powerqueue task retry ENG-123`.
- `run --offline` or `linear.enabled = false` disables polling. Manual tasks
  (`powerqueue add`) still work.
- Tasks exist but stay `queued`: slots are full (`scheduler.max_concurrent`),
  or every tier is throttled (see below). A task that is `paused` with the
  last error `skipped by PRIORITY.md` has a `KEY: skip` line in
  `## Overrides` (`powerqueue task explain ENG-123`).

## A task keeps crashing

**Symptom**: state cycles `starting` → `crashed`, then `failed` after
`scheduler.max_attempts`.

Checks:

```sh
powerqueue task show ENG-123           # timeline: exit codes, errors, attempts
powerqueue task output ENG-123 -n 200  # last screen of the pane (kept by remain-on-exit)
powerqueue logs --task ENG-123 --level warn
cat ~/.local/state/powerqueue/tasks/<task-id>/launch.sh
```

Fixes:

- **Setup commands fail**: `repo.setup` (or `.powerqueue.toml` `setup`) runs
  with `sh -c` in the fresh worktree before Claude starts; the first failing
  command aborts the attempt (`session.crashed` event with
  `worktree setup failed: ...` and the command output). Run the command by
  hand inside the worktree.
- **Claude exits immediately**: run `sh launch.sh` yourself in a terminal to
  see the real error (the pane's last 40 lines are also in the
  `session.crashed` event). Common causes: not logged in
  (`claude auth status`), an invalid `claude.extra_args`, a `claude.binary`
  that is not on the daemon's `PATH` (launchd and systemd have a minimal
  `PATH`; set it in the unit).
- **Permission prompts**: in `permission_mode = "default"` a prompt blocks the
  session until a human answers. Use `acceptEdits` (default) or
  `bypassPermissions` for unattended runs, or add `claude.allowed_tools`.
- **Resume loops**: after a crash the daemon relaunches with
  `--resume <session id>`. If the transcript itself is corrupt, cancel and
  retry to get a fresh session: `task cancel` then `task retry`.
- **Wall-clock cap**: `scheduler.max_session_secs` (default 4h) kills long
  attempts (`session.timeout` event, counted as a crash and resumed after the
  backoff); raise it or split the ticket. Likewise `stale_session_secs`
  (default 30 min without hook events or transcript growth, `session.stale`).

## Stuck in `needs_attention`

**Symptom**: the task is waiting for a human.

Why it happens: Claude printed `[[POWERQUEUE:BLOCKED]]` or ran
`task block`, its last message read like a question (ends with `?`, "should
I", "let me know", ...), it hit a permission prompt (`Notification` hook with
`permission_prompt`), a `StopFailure` reported `authentication_failed`, or
it was idle longer than `scheduler.idle_timeout_secs` after a nudge.

```sh
powerqueue task show ENG-123           # the reason is the last event / "last error"
powerqueue attach ENG-123              # answer in the session
powerqueue task send ENG-123 "Use the v2 endpoint, then finish."
powerqueue task resume ENG-123         # back to running (queued if the session is gone)
```

`task send` types into the pane and presses Enter; it refuses when the
session is no longer live. `task resume` moves the task to `running` while
the session is alive, else to `queued` for a relaunch; `task retry` starts a
fresh attempt with a new session.

## Throttled all the time

**Symptom**: tasks sit in `throttled` with a `retry_at`.

Checks:

```sh
powerqueue budget show                 # spend per tier, window, cooldowns
powerqueue task explain ENG-123        # why each tier was rejected
powerqueue doctor                      # anchor, calibration, throttling rate
```

Fixes:

- No `period_anchor`: `budget set-reset <time from /usage>`, or let the
  probe learn it (`doctor`'s `<provider> budget anchor` check says whether the
  anchor comes from config, from a probe, or is the Monday default). Without
  it the period boundary is a guess.
- Spend looks too high compared with `/usage`: `budget set-observed 43%`
  corrects the offset; or raise `budget.providers.claude.period_weighted_tokens` /
  `window_weighted_tokens`.
- A tier is on cooldown after a rate limit: wait, or
  `budget clear-limits` once `/usage` confirms the window reset.
- Only Fable/Opus are preferred and both are reserved: lower
  `min_criticality`, lower `relax_after_fraction`, or let the policy choose by
  removing the `## Models` line.
- Too many parallel sessions drain the window: lower
  `scheduler.max_concurrent`.

See [budget.md](budget.md) for the algorithm.

## tmux session or window missing

**Symptom**: `attach` says there is no session, or a task is `running` but no
window exists.

```sh
tmux ls                                # or: tmux -L <socket_name> ls
powerqueue config show | sed -n '/\[tmux\]/,/^\[/p'
powerqueue doctor --fix                # marks tasks with no live session crashed
powerqueue attach ENG-123 --print      # the exact tmux command (socket, window id)
```

- `tmux.socket_name` must match: the daemon and your shell must use the same
  `-L` socket. Unset it unless you need isolation.
- The server was killed (`tmux kill-server`, reboot). Running sessions are
  gone; the daemon marks them `crashed` on the next probe and relaunches
  with `--resume`, so context survives.
- `cleanup.close_tmux_window = true` (default) removes the window on
  completion; set it to `false` to keep the final output on screen.
- tmux must be ≥ 3.2; `doctor` prints `tmux -V` but does not enforce the
  version.

## A worktree was left behind

**Symptom**: `<worktree_root>/<slug>` still exists after completion.

```sh
powerqueue task show ENG-123           # look for "cleanup" events
git -C <repo> worktree list
```

- The push failed or there is no remote: the daemon never deletes unpushed
  work. Push by hand, then `git worktree remove <path>`.
- The task failed and `cleanup.keep_failed = true` (default) kept it for
  inspection.
- A `cleanup.run` command failed; the worktree is kept for inspection.
- `cleanup.remove_worktree = false` globally or in `.powerqueue.toml`.
- Orphans (no open task references them): `powerqueue doctor --fix` removes
  the clean, pushed ones and lists the rest.

## I want to start over

**Symptom**: a bad first run left tasks, worktrees and tmux windows you
would rather forget.

```sh
powerqueue reset --dry-run        # shows exactly what would go
powerqueue reset                  # asks, then does it (-y skips the prompt)
```

`reset` stops the daemon if it is running (or tells you to `powerqueue stop`
when it will not go), kills the task windows and the `powerqueue` tmux
session, removes every task worktree and stray directory under the worktree
root, deletes `state/tasks/*` and empties every table of the database. It
never touches `config.toml`, secrets, `PRIORITY.md` or the logs, and nothing
outside the worktree root, the task state directory and the database is
deleted.

- Worktrees with uncommitted changes or unpushed commits are **kept and
  listed** with the reason; `--force` removes them anyway.
- Local `pq/*` branches stay unless you pass `--delete-branches` (the default
  branch and anything not matching `repo.branch_template` are always kept;
  remote branches are never deleted).
- Budget calibration, rate-limit cooldowns and probe results (the `kv` table)
  survive so pacing keeps its learning; `--everything` clears them too.
- Linear issues are left as they are; `--revert-linear` moves the issues of
  open tasks back to the first `linear.queued_states` entry (needs the Linear
  key; failures are reported per issue).

## Keys not found

**Symptom**: `Linear API key not configured`, or `secrets list` shows nothing
although you set a key.

```sh
powerqueue secrets list                # name, present?, origin (environment|keychain|file)
echo "$POWERQUEUE_SECRETS"
```

Precedence is environment (`LINEAR_API_KEY`, `JEV_API_KEY`) → keychain → file.

- The key was stored in the keychain but the daemon runs under launchd/systemd
  without keychain access. Either grant access when prompted, set the
  environment variable in the unit, or use the file backend for the daemon:
  `POWERQUEUE_SECRETS=file powerqueue secrets set linear` and start the
  daemon with the same variable.
- Linux without Secret Service (headless): the file backend
  (`<config>/secrets.toml`, mode 0600) is chosen automatically; `doctor`
  says so.
- An empty environment variable is ignored, a whitespace-only value is not a
  key.

## Rate limit errors

**Symptom**: `budget.rate_limited` events (`claude (opus) reported
rate_limit; cooling down every claude model until ...`), tasks `throttled`.

The provider of the session's model goes on cooldown for
`budget.providers.<provider>.rate_limit_cooldown_mins` (never past that
provider's period end) and the task is throttled; if its session is still
alive when the cooldown passes it resumes as `running`. Other enabled
providers keep taking work. This is expected; it means the subscription
window is full. Check `/usage` inside Claude Code (or `powerqueue budget
probe`), run `budget set-observed`, and consider lowering
`scheduler.max_concurrent`. `budget clear-limits [--provider P]` lifts the
cooldown early.

A Claude pane that shows `Usage limit reached · continuing automatically at
…` is waiting for the reset, not hung: the daemon logs
`session.waiting_for_reset` and leaves it alone until the cooldown ends.

## A provider is never picked

**Symptom**: you enabled Codex (or Gemini) but every task still runs on
Claude, or tasks wait in `throttled` although another provider has room.

Checks:

```sh
powerqueue config get budget.provider_order   # fallback order
powerqueue budget show                        # one section per enabled provider
powerqueue budget probe                       # what each provider reports now
powerqueue task explain ENG-123               # per-model verdicts and the choice
```

Fixes:

- The provider is not enabled: `budget.providers.codex.enabled = true`
  (`budget show` lists disabled providers at the end).
- It is enabled but comes later in `budget.provider_order`: a provider is
  only a fallback for tasks whose preferences it does not match. Put it
  first, or name its models in `PRIORITY.md` (`critical: gpt-6.1-sol | fable`).
- The task has a hard override (`task model <task> opus`): it never crosses
  providers. Clear it with `task model <task> auto`.
- Every model of the provider is disabled or has `share = 0`, or its
  `min_criticality` reserves it (the verdict says `reserved for critical`).
- The provider is on cooldown: the verdict says `rate limited until …` or
  `codex reports its allowance blocked; skipped until the reset at …`.
  `budget probe --provider codex` re-reads the real state; `budget
  clear-limits --provider codex` forgets a stale rate-limit mark.
- The probe fails (`budget show` prints `probe last run … failed`; `doctor`'s
  `codex usage probe` check warns): log in (`codex login`), check
  `codex.binary`, or set `probe_interval_mins = 0` to rely on measurements
  only. The same check warns when the last observation is older than
  3 × `budget.probe_interval_mins` (usually: the daemon is not running) and
  says `no probe yet` before the first one.
- `doctor` runs the provider checks for every enabled provider: `codex` /
  `gemini` (binary on PATH and its version), `codex auth` / `gemini auth`
  (logged in: `codex login status`; `agy` sign-in), `<provider> models`
  (shares sum to at most 1 and at least one model is enabled), `<provider>
  budget anchor`, `<model> reservation` (pacing of the provider's most
  capable model, e.g. `gpt-6.1-sol reservation`) and `<provider> window
  pressure`. Claude keeps its old names (`claude`, `claude auth`, `fable
  reservation`, `window pressure`). `gemini` always carries an
  "experimental" warning.
- You forced a model of a disabled provider (`task model ENG-1 gpt-6-astra`
  printed `warning: codex is disabled in config`): enable the provider or
  clear the override; the task waits otherwise.
- Codex Pro-family plans have no 5-hour window: set
  `budget.providers.codex.window_hours = 0` so the window estimate never
  blocks it.

## `powerqueue tune` did not apply anything

**Symptoms**: `tune` prints `failed:`, `invalid:` or `proposed:`; `doctor`
warns about tune drafts.

**Checks**

- `failed: ... exited with N` or `timed out`: read `stderr.log` and
  `result.json` in the draft directory it names (`<state>/tune/<id>/`).
  Common causes: `claude` not logged in (`claude auth status`), the model in
  `tune.model` / `-m` not available on your plan, a nested session refusing to
  start, or a slow run hitting `tune.timeout_secs` (default 600 s).
- `invalid:`: Claude wrote something `PRIORITY.md` or `config.toml` cannot
  parse; the problems are listed with line numbers. The live files were not
  touched. Fix the draft by hand and `powerqueue tune --apply <dir>`, or
  re-run with a more specific request.
- `proposed:` (exit 3): there was no terminal to ask on and no `-y`. Review
  with `powerqueue tune --apply` (it shows the diff and the simulation again)
  or pass `-y` from scripts.
- Claude "changed nothing": its summary explains why, usually because the
  request is decided by the budget policy, not the rules (try `powerqueue
  budget show`), or because the ticket is not in the queue yet (`powerqueue
  linear sync`).

**Fix**: a wrong apply is reverted with `powerqueue tune --undo` (the
`original/` copies are restored and a running daemon reloads). Finished
drafts beyond `tune.keep_drafts` are pruned automatically; delete a draft
directory to silence `doctor`.

## Where things are

| What | Path |
|------|------|
| config, PRIORITY.md, secrets.toml | `~/.config/powerqueue/` |
| database | `~/.local/share/powerqueue/powerqueue.db` (+ `-wal`, `-shm`) |
| worktrees | `~/.local/share/powerqueue/worktrees/<repo>/<slug>` |
| logs | `~/.local/state/powerqueue/logs/powerqueue.log.<date>` |
| per-task files | `~/.local/state/powerqueue/tasks/<task-id>/{prompt.md,settings.json,launch.sh,env}` |
| lock, pid | `~/.local/state/powerqueue/{daemon.lock,daemon.pid}` |
| Claude transcripts | `~/.claude/projects/<encoded cwd>/<session id>.jsonl` (`CLAUDE_CONFIG_DIR` replaces `~/.claude`) |

`POWERQUEUE_HOME=/x` moves everything to `/x/{config,data,state}`.
`XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_STATE_HOME` are honoured.

## The dashboard shows garbage, squares, or does not take input

**Symptom**: `powerqueue dashboard` paints boxes/squares or stray characters,
the keys do nothing, or the shell prompt is still visible underneath.

Checks:

```sh
powerqueue doctor                      # "terminal" check: TERM and locale
echo "$TERM"; locale charmap           # want a real TERM and UTF-8
tty; echo "$TMUX"                      # must be a terminal; inside tmux?
powerqueue dashboard --once            # one frame as text, no TTY needed
```

- The live dashboard needs stdin **and** stdout to be a terminal and a usable
  `TERM`. Started from a pipe, a `cron`/launchd job, an editor's embedded
  console or an agent's shell tool, it now exits 2 with
  `the dashboard needs an interactive terminal …` instead of writing escape
  sequences into the pipe. Use `powerqueue status`, or `dashboard --once`
  (`--json` for the raw snapshot).
- Without `config.toml` it fails with the same "run `powerqueue init`" error
  as `status`; check `POWERQUEUE_HOME`/`--home` if the setup seems lost.
- Squares (tofu) where the status dot `●`, the row marker `▶`, the cursor
  `█` or the box-drawing borders should be: the terminal font has no glyph
  for them, or the locale is not UTF-8 (`LANG=C`). Fix the font (Ghostty
  falls back to a Nerd Font symbols set automatically; other terminals need
  a font with box-drawing glyphs), set `LANG=en_US.UTF-8`, or run
  `powerqueue dashboard --ascii`, which uses `*`, `>`, `#` and `+-|`
  borders. A non-UTF-8 locale selects `--ascii` automatically.
- Rectangles in the budget panel: the gauges are drawn with background
  colours; on a light theme they looked like black bars (fixed in
  `[Unreleased]`). `--ascii` draws `[####....]` text bars instead.
- Inside tmux, `a`/`enter` switches the tmux client to the task's window and
  the dashboard keeps running in its own window; outside tmux it replaces the
  dashboard with `tmux attach`.
- Still wrong? Attach `powerqueue dashboard --once`, `echo $TERM`,
  `locale`, the terminal name/version and `tmux -V` to the bug report.

## Reporting a bug

Attach these, with secrets removed (they are never printed, but check):

```sh
powerqueue --version
powerqueue doctor --offline --json > doctor.json
powerqueue task show ENG-123 --json > task.json
powerqueue logs -n 500 --task ENG-123 > task.log
powerqueue config show > config.toml          # review before sharing
```

Open an issue with the bug report template. Mention OS, tmux version
(`tmux -V`) and Claude Code version (`claude --version`).

## The session sits on a "trust this folder?" or permission dialog

**Symptom**: `task output ENG-123` shows Claude Code's workspace-trust dialog,
or a "Do you want to proceed?" prompt, and the task is `needs_attention`.

- The trust dialog should not appear: with `claude.trust_workspace = true`
  (default) powerqueue marks the repository root and the worktree as trusted in
  `~/.claude.json` (`projects["<path>"].hasTrustDialogAccepted`) before every
  launch. If you set `CLAUDE_CONFIG_DIR`, the file is
  `$CLAUDE_CONFIG_DIR/.claude.json`. Check the daemon log for
  `could not pre-trust workspace`.
- Permission prompts are expected in `acceptEdits` mode for shell commands
  other than `powerqueue task …` (always allowed). Either attach and answer
  (`powerqueue attach ENG-123`, choose "don't ask again" where sensible), add
  the commands your repo needs to `claude.allowed_tools`, or switch
  `claude.permission_mode` to `auto` or `bypassPermissions` for unattended runs.
