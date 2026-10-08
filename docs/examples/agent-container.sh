#!/bin/sh
# Start, stop and enter the per-task container that runs a powerqueue
# session. Copy it into your repository (e.g. tools/agent-container.sh) and
# wire it up in config.toml or .powerqueue.toml:
#
#   [repo]
#   setup = ["tools/agent-container.sh up"]
#   [cleanup]
#   run = ["tools/agent-container.sh down"]
#   [claude]
#   binary = "tools/agent-container.sh exec {slug} {task_dir} {worktree} claude"
#   shim = true
#   [claude.env]
#   CLAUDE_CONFIG_DIR = "/home/me/.local/share/powerqueue/agent-home/claude"
#   [codex]
#   binary = "tools/agent-container.sh exec {slug} {task_dir} {worktree} codex"
#   shim = true
#   [codex.env]
#   CODEX_HOME = "/home/me/.local/share/powerqueue/agent-home/codex"
#
# `up` and `down` run with powerqueue's setup/cleanup environment
# (POWERQUEUE_TASK_SLUG, POWERQUEUE_WORKTREE, POWERQUEUE_DATA_DIR,
# POWERQUEUE_STATE_DIR, ...) in the worktree; `exec` runs the CLI inside the
# container with the task's env file and working directory. Every path is
# mounted at the same absolute path inside the container, which is what
# keeps the task directory (shim, inbox, settings), the worktree and the
# CLI's transcripts valid on both sides. See docs/containers.md.
set -eu

IMAGE="${PQ_AGENT_IMAGE:-powerqueue-agent}"
# Where the CLIs keep their logins and transcripts: claude.env.CLAUDE_CONFIG_DIR
# and codex.env.CODEX_HOME point below it.
AGENT_HOME="${PQ_AGENT_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/powerqueue/agent-home}"

cmd="${1:-}"
[ $# -gt 0 ] && shift
case "$cmd" in
  up)
    name="pq-$POWERQUEUE_TASK_SLUG"
    # The worktree's .git file points into the main checkout: mount both.
    repo="$(cd "$POWERQUEUE_WORKTREE" && git rev-parse --path-format=absolute --git-common-dir)"
    repo="${repo%/.git}"
    mkdir -p "$AGENT_HOME"
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker run -d --name "$name" \
      -v "$POWERQUEUE_DATA_DIR:$POWERQUEUE_DATA_DIR" \
      -v "$POWERQUEUE_STATE_DIR:$POWERQUEUE_STATE_DIR" \
      -v "$repo:$repo" \
      -v "$AGENT_HOME:$AGENT_HOME" \
      -w "$POWERQUEUE_WORKTREE" \
      -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0='*' \
      "$IMAGE" sleep infinity >/dev/null
    echo "agent-container: $name is up ($IMAGE)"
    ;;
  down)
    docker rm -f "pq-$POWERQUEUE_TASK_SLUG" >/dev/null 2>&1 || true
    ;;
  exec)
    # exec <slug> <task dir> <worktree> <cli> [cli args...]
    slug="$1"; task_dir="$2"; worktree="$3"; shift 3
    name="pq-$slug"
    # A container that stopped (reboot) would make every relaunch crash:
    # start it again when it exists.
    docker start "$name" >/dev/null 2>&1 || true
    exec docker exec -it --env-file "$task_dir/env" -w "$worktree" "$name" "$@"
    ;;
  *)
    echo "usage: $0 up | down | exec <slug> <task dir> <worktree> <cli> [args...]" >&2
    exit 2
    ;;
esac
