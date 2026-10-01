# Troubleshooting

Start with `powerqueue doctor`. It checks the environment, configuration,
secrets, database state and the scheduling algorithm, and prints a fix hint for
every problem. `doctor --fix` applies the safe repairs (prune orphan
worktrees and windows, reset stuck states, clear stale locks).

Each entry below is symptom → checks → fix.

## The daemon will not start

**Symptom**: `powerqueue run` exits with a lock or pid error.

Checks:

```sh
powerqueue config path                       # find the state dir
cat ~/.local/state/powerqueue/daemon.pid     # pid of the last daemon
ps -p "$(cat ~/.local/state/powerqueue/daemon.pid)"
powerqueue doctor                            # "daemon heartbeat" check
```

The daemon takes an exclusive lock on `<state>/daemon.lock` and writes its pid
to `daemon.pid`. Only one daemon per data directory can run.

Fixes:

- Another daemon is alive: use it (`powerqueue dashboard`) or stop it with
  `powerqueue stop`.
- The pid is dead but the lock remains: `powerqueue doctor --fix` clears a
  stale lock. Or delete `daemon.lock` and `daemon.pid` by hand.
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
  or every tier is throttled (see below), or an `## Overrides` line says
  `skip` (`powerqueue task explain ENG-123`).

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
  command aborts the attempt and its output is in the timeline. Run the
  command by hand inside the worktree.
- **Claude exits immediately**: run `launch.sh` yourself in a terminal to see
  the real error. Common causes: not logged in (`claude auth status`), an
  invalid `claude.extra_args`, a `claude.binary` that is not on the daemon's
  `PATH` (launchd and systemd have a minimal `PATH`; set it in the unit).
- **Permission prompts**: in `permission_mode = "default"` a prompt blocks the
  session until a human answers. Use `acceptEdits` (default) or
  `bypassPermissions` for unattended runs, or add `claude.allowed_tools`.
- **Resume loops**: after a crash the daemon relaunches with
  `--resume <session id>`. If the transcript itself is corrupt, cancel and
  retry to get a fresh session: `task cancel` then `task retry`.
- **Wall-clock cap**: `scheduler.max_session_secs` (default 4h) kills long
  attempts; raise it or split the ticket.

## Stuck in `needs_attention`

**Symptom**: the task is waiting for a human.

Why it happens: Claude printed `[[POWERQUEUE:BLOCKED]]` or ran
`task block`, asked a question, hit a permission prompt
(`Notification` hook with `permission_prompt`), or was idle longer than
`scheduler.idle_timeout_secs` after a nudge.

```sh
powerqueue task show ENG-123           # the reason is the last event
powerqueue attach ENG-123              # answer in the session
powerqueue task send ENG-123 "Use the v2 endpoint, then finish."
powerqueue task resume ENG-123         # back to running
```

If the session is gone, `task retry` starts a fresh attempt.

## Throttled all the time

**Symptom**: tasks sit in `throttled` with a `retry_at`.

Checks:

```sh
powerqueue budget show                 # spend per tier, window, cooldowns
powerqueue task explain ENG-123        # why each tier was rejected
powerqueue doctor                      # anchor, calibration, throttling rate
```

Fixes:

- No `period_anchor`: `budget set-reset <time from /usage>`. Without it the
  period boundary is a guess.
- Spend looks too high compared with `/usage`: `budget set-observed 43%`
  corrects the offset; or raise `budget.period_weighted_tokens` /
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
powerqueue doctor --fix                # resets tasks with no live session
```

- `tmux.socket_name` must match: the daemon and your shell must use the same
  `-L` socket. Unset it unless you need isolation.
- The server was killed (`tmux kill-server`, reboot). Running sessions are
  gone; the daemon marks them `crashed` on the next probe and relaunches
  with `--resume`, so context survives.
- `cleanup.close_tmux_window = true` (default) removes the window on
  completion; set it to `false` to keep the final output on screen.
- tmux must be ≥ 3.2; `doctor` checks `tmux -V`.

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
- `cleanup.remove_worktree = false` globally or in `.powerqueue.toml`.
- Orphans (no task references them): `powerqueue doctor --fix` prunes them.

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

**Symptom**: events `hook.stop_failure` with `rate_limit` or `overloaded`.

The tier goes on cooldown for `budget.rate_limit_cooldown_mins` and the task
is throttled. This is expected; it means the subscription window is full.
Check `/usage` inside Claude Code, run `budget set-observed`, and consider
lowering `scheduler.max_concurrent`. `budget clear-limits` lifts the cooldown
early.

## Where things are

| What | Path |
|------|------|
| config, PRIORITY.md, secrets.toml | `~/.config/powerqueue/` |
| database | `~/.local/share/powerqueue/powerqueue.db` (+ `-wal`, `-shm`) |
| worktrees | `~/.local/share/powerqueue/worktrees/<repo>/<slug>` |
| logs | `~/.local/state/powerqueue/logs/powerqueue.log.<date>` |
| per-task files | `~/.local/state/powerqueue/tasks/<task-id>/{prompt.md,settings.json,launch.sh,env}` |
| lock, pid | `~/.local/state/powerqueue/{daemon.lock,daemon.pid}` |
| Claude transcripts | `~/.claude/projects/<encoded cwd>/<session id>.jsonl` |

`POWERQUEUE_HOME=/x` moves everything to `/x/{config,data,state}`.
`XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_STATE_HOME` are honoured.

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
