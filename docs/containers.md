# Running sessions in containers

powerqueue can run each task's agent session inside a container (or any
other place where the daemon's own `powerqueue` binary cannot run) while
keeping everything else as it is: the tmux pane is the CLI's terminal,
`powerqueue attach` and `task send` still work, crashes still resume the same
session, the transcript is still tailed for usage, and `task complete` still
ends the task. Two settings make it possible, and a third one is usually
needed:

| Setting | What it does |
|---------|--------------|
| `<provider>.binary` as a command template | `claude.binary = "docker exec -it --env-file {task_dir}/env -w {worktree} pq-{slug} claude"`: the command line is split like a shell would, the placeholders are filled per task, and the provider's own flags and the prompt are appended. The first word is the program. |
| `<provider>.shim = true` | The session cannot run the host binary, so hooks, Claude Code's status line and `powerqueue task complete\|block` go through a POSIX shell shim written to `<task dir>/bin/powerqueue`. The shim drops one file per call into `<task dir>/inbox/`; the daemon drains it every tick and applies each message as if the binary had been run. |
| `claude.env.CLAUDE_CONFIG_DIR` / `codex.env.CODEX_HOME` | Where the CLI keeps its login and transcripts, as a path that is the same on the host and inside the container. The daemon reads transcripts and seeds workspace trust there; the container gets it through the task's env file. |

Placeholders for `binary`: `{key}`, `{slug}`, `{task_id}`, `{session_id}`,
`{worktree}`, `{task_dir}`, `{repo}`, `{attempt}`, `{model}`. Values are
inserted after splitting, so a worktree path with spaces needs no quoting.
`config validate` and `doctor` reject unknown placeholders and unbalanced
quotes.

## The one rule: same paths on both sides

Mount these at the **same absolute path** inside the container:

- the state directory (`<state>/tasks/<task id>/` holds the prompt,
  `settings.json`, the env file, the shim and the inbox; `powerqueue doctor`
  prints the paths),
- the worktree root (the data directory) **and** the main checkout (a
  worktree's `.git` file points into the main repository's `.git/worktrees/`),
- the directory under `CLAUDE_CONFIG_DIR` / `CODEX_HOME`.

Then `--settings <path>`, `$(cat <prompt>)`, `-C <worktree>`, the hook
commands in `settings.json` and the transcript path Claude Code derives from
its working directory are all valid in both places, and nothing needs to be
rewritten. Use physical paths (no symlinks): Claude Code names the
transcript after the resolved working directory, and the daemon resolves the
worktree path on the host the same way, so a worktree under macOS's `/tmp`
(really `/private/tmp`) would be looked up under a name the container never
writes.

## Recipe with Docker

1. Build an image with the CLIs (and git):
   [docs/examples/Dockerfile.agent](examples/Dockerfile.agent) installs
   Claude Code and Codex on Node 22.

   ```sh
   docker build -t powerqueue-agent -f docs/examples/Dockerfile.agent docs/examples
   ```

2. Pick a directory for the CLIs' state and log in once per CLI. The
   subscription login (OAuth) persists there, so sessions never see a login
   prompt:

   ```sh
   AGENT_HOME=~/.local/share/powerqueue/agent-home
   mkdir -p $AGENT_HOME/claude $AGENT_HOME/codex
   docker run -it --rm --user "$(id -u):$(id -g)" -e HOME=$AGENT_HOME -v $AGENT_HOME:$AGENT_HOME \
     -e CLAUDE_CONFIG_DIR=$AGENT_HOME/claude powerqueue-agent claude auth login
   docker run -it --rm --user "$(id -u):$(id -g)" -e HOME=$AGENT_HOME -v $AGENT_HOME:$AGENT_HOME \
     -e CODEX_HOME=$AGENT_HOME/codex powerqueue-agent codex login
   ```

   Run the containers as your own user (`--user`, as the helper does): on
   rootful Docker on Linux the agent would otherwise leave root-owned
   sources and git objects on the bind mounts that the daemon cannot clean
   up. `HOME` is set to the mounted agent home so the CLIs can write their
   state.

3. Copy [docs/examples/agent-container.sh](examples/agent-container.sh) into
   your repository (e.g. `tools/agent-container.sh`). It starts a container
   named `pq-<slug>` per task with the mounts above (`up`), removes it
   (`down`), and runs the CLI inside it (`exec`).

4. Configure powerqueue (global `config.toml`, or `.powerqueue.toml` in the
   repository for `setup` and `cleanup.run`):

   ```toml
   [repo]
   setup = ["tools/agent-container.sh up"]

   [cleanup]
   run = ["tools/agent-container.sh down"]

   [claude]
   binary = "tools/agent-container.sh exec {slug} {task_dir} {worktree} claude"
   shim = true
   [claude.env]
   CLAUDE_CONFIG_DIR = "/home/me/.local/share/powerqueue/agent-home/claude"

   [codex]
   binary = "tools/agent-container.sh exec {slug} {task_dir} {worktree} codex"
   shim = true
   [codex.env]
   CODEX_HOME = "/home/me/.local/share/powerqueue/agent-home/codex"
   ```

   Without the helper script, `binary` can call Docker directly:
   `docker exec -it --env-file {task_dir}/env -w {worktree} pq-{slug} claude`.
   `--env-file` carries the task's environment (`POWERQUEUE_TASK_ID`,
   `POWERQUEUE_SESSION_ID`, `POWERQUEUE_BIN`, `CLAUDE_CONFIG_DIR`, your
   `claude.env`) into the container, since `docker exec` forwards nothing by
   itself.

5. `powerqueue doctor`: the provider line reports the per-task command and
   where its program (`docker`, or the script) was found; the login check is
   skipped with a note, because it can only be checked inside the container.
   `config validate` checks the template.

`repo.setup` and `cleanup.run` get `POWERQUEUE_TASK_SLUG`,
`POWERQUEUE_TASK_DIR`, `POWERQUEUE_DATA_DIR` and `POWERQUEUE_STATE_DIR` in
addition to the task id, key, branch and worktree, so a script can name the
container and know what to mount. Cleanup runs `down` when the task
completes, fails, is cancelled or is handed off for review; a review round
recreates the worktree and runs `setup` (and so `up`) again.

## What the session sees

- `settings.json` (Claude Code) wires every hook and the status line to
  `<task dir>/bin/powerqueue hook …`; Codex gets the same path as its
  `notify` program; Antigravity's `.agents/hooks.json` too.
- The prompt's completion protocol names the shim by its absolute path
  (`/…/tasks/<id>/bin/powerqueue task complete <id> --summary "…"`), and
  Claude sessions get `Bash(<that path> task *)` in `--allowedTools` next to
  the usual `Bash(powerqueue task *)`. A custom prompt template can use
  `{{powerqueue}}` for it.
- `POWERQUEUE_BIN` in the session environment is that same path, for your
  own scripts and hooks.
- The shim knows `hook`, `task complete <task> [--summary …] [--pr …]`,
  `task block <task> [--reason …]` and `--version`. Anything else fails with
  a message saying so (exit 2). A hook never fails (a non-zero hook exit
  would stop the agent's turn); when it cannot write, it says so on stderr
  and exits 0.
- The inbox is `<task dir>/inbox/`: one `<seq>-<epoch>-<pid>-<kind>.msg` per
  call (the sequence number comes from the inbox's `.seq` counter, bumped
  under a `mkdir` lock since the status line runs alongside hooks, so the
  daemon applies messages in the order they were written), a JSON header
  line (`{"kind":"complete","task":"…","pr":null,"session":"…"}`) followed by
  the body (the summary, the reason, or the hook's JSON payload). Files are
  written under a temporary name and renamed, so the daemon never reads a
  partial one. A `task complete|block` that cannot write the inbox exits 2
  and says so.

## What the daemon does

Each tick, before the hook phase, it drains the inbox of every non-terminal
task when any provider has `shim = true`:

- `hook` messages become `hook_events` rows (the same path `powerqueue hook`
  takes, provider payloads normalised the same way) and are handled by the
  hook phase of the same tick; Claude Code's `StatusLine` payloads update the
  observed usage like the status-line command does;
- `complete` messages run the same code as `powerqueue task complete`
  (`--pr` hands off for review), logged as `inbox.complete`;
- `block` messages run `powerqueue task block` and record the agent's
  question for the Linear relay, logged as `inbox.block`;
- a message that cannot be applied (unknown task, a task in a state that
  cannot complete) is logged as `inbox.error` and dropped; an unreadable
  file is moved to `inbox/rejected/`, logged as `inbox.rejected`, and
  `doctor` reports how many are parked;
- when the session probe finds a pane dead, it drains that task's inbox
  first, so a `task complete` the session wrote right before exiting is
  applied instead of the exit being read as a crash;
- a message from a session that already ended (its `session` id, or the
  task's latest session when the shim had none) is ignored and logged as
  `inbox.stale`: a Stop that arrived after `task complete` and was only
  drained once the task was re-queued must not complete the next attempt
  before it starts. A launch also discards whatever the previous session
  left in the inbox.

Everything else is unchanged: tmux liveness, `--resume` after a crash (the
relaunch runs the same `docker exec` against the same container, so keep the
container up between attempts; the helper script starts a stopped one), the
transcript tailer (reading the mounted `CLAUDE_CONFIG_DIR` / `CODEX_HOME`),
cleanup and Linear updates.

## Limitations

- CPU and memory samples come from the pane's process on the host, which is
  the `docker exec` client, not the agent. Usage (tokens) is still exact.
- Claude Code's status line shows nothing in the pane (the shim prints no
  text); the rate-limit snapshot it carries still reaches the daemon.
- `powerqueue tune` runs Claude Code on the host and refuses a per-task
  `claude.binary`; set it to a plain command while tuning.
- Docker Desktop on macOS shares `/Users`, `/tmp` and `/private` by default;
  a state directory elsewhere must be added to its file sharing list.
- Codex's `workspace-write` sandbox applies inside the container as usual;
  the data and state directories are passed with `--add-dir` and exist at the
  same paths, so `task complete` through the shim works.

## Testing

`tests/e2e_daemon.rs` has a host-only end-to-end test of the shim
(`shim_routes_hooks_and_completion_through_the_inbox`: the fake CLI finds the
shim on PATH, hooks and completion go through the inbox) that runs with the
rest of the suite, and two Docker tests
(`container_claude_session_completes_through_docker_exec`,
`container_codex_session_completes_through_docker_exec`) that build a small
Alpine image with the fake CLIs and run the real daemon against `docker
exec`. The Docker tests run only with `POWERQUEUE_E2E_DOCKER=1` and a
working `docker`; they need network access for the first image build.
