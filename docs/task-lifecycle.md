# Task lifecycle

[Documentation](README.md) · [Project overview](../README.md)

The queue and worktree lifecycle are shared across providers. The launch and
hook examples below describe Claude Code, the default provider; see the
[provider settings](configuration.md#providers) for Codex and experimental
Antigravity differences. [Container sessions](containers.md) use the same
lifecycle, with hooks and completion commands delivered through the task inbox.

1. The daemon picks the highest-scoring schedulable task and asks the budget
   policy for a model (see [docs/budget.md](budget.md)).
2. It runs `git fetch --prune origin` (`repo.fetch_before_start`) and creates
   `<worktree_root>/<slug>` on branch `pq/<slug>` (configurable), then runs
   `repo.setup` commands there. A new branch starts from the freshly fetched
   `origin/<default_branch>`, not the local branch, so work merged on GitHub
   (a `blocked by` blocker's PR) is in it even when the main checkout's
   `main` lags; it does not track the base, so a bare `git push` never
   targets `main`. Without an `origin` (or with the fetch turned off) the
   local branch is used. If the fetch fails the branch starts from the newer
   of the last fetched `origin/<default_branch>` and the local branch, and a
   `worktree.stale_base` warning is logged (`doctor` counts them). An
   existing branch (a relaunch, a review round) is reused as is. The base is
   recorded when the branch is created (`worktree.branch_created`, also on
   `worktree.ready`) and `powerqueue task show` prints it
   (`base  origin/main at <sha>`). Before creating a branch, the local
   default branch is fast-forwarded to `origin/<default_branch>`
   (`repo.fast_forward_base`) when that loses nothing: it has no commits of
   its own, no rebase or bisect of it is in progress, and, if checked out,
   that checkout has no uncommitted changes to tracked files. The repository's
   git hooks do not run for it.
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
   `PreCompact`, `UserPromptSubmit`, `PostToolUse`, `PostToolUseFailure`) call `powerqueue hook`, which stores the
   payload in SQLite. The daemon drains these, tails the transcript JSONL for
   token usage (deduplicated by message id), and samples CPU/RSS.

## The prompt protocol

The prompt (`src/session/launcher.rs::build_prompt`) contains the key and
title, the description, the source issue link, the working rules (stay on the
branch, commit as you go, do not push), the completion protocol and, when
set, `prompt.instructions`. A `[prompt] template` replaces the whole text
with your own Markdown (see [`[prompt]`](configuration.md#prompt)); `powerqueue task prompt
<task>` shows what a task would receive. The protocol itself:

- **Done**: run `powerqueue task complete <id> --summary "..."` and print
  `[[POWERQUEUE:DONE]]` as the last line of the final message. Either one on
  its own completes the task; text after the marker becomes the summary.
- **Blocked**: run `powerqueue task block <id> --reason "..."` and print
  `[[POWERQUEUE:BLOCKED]]`. The task moves to `needs_attention` and waits for a
  human (`powerqueue attach <task>` or `task send <task> "reply"`). A direct
  question on the final line is treated the same way; courtesy closings such
  as "let me know" are not blockers. A submitted reply clears attention and
  its old reason. Permission requests also clear on a completed tool call or
  newer assistant response; explicit blockers still require a reply.
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
- **Review**: a session that opens a pull request and arms its merge runs
  `powerqueue task complete <id> --pr <url>` instead (then the done marker).
  The task becomes `in_review` and the session ends; see below. A session
  that completes without `--pr` while its branch has an open PR
  (`gh pr list --head pq/<slug>`) is handed off for review with that PR too
  (also at the end of a review round).
- **Waiting**: while a task is `needs_attention` (a question, a permission
  prompt), paused or throttled with its session alive, neither
  `stale_session_secs` nor `max_session_secs` ends the session, and a
  context compaction does not clear a question. `max_session_secs` counts
  only the time the agent worked.
- A later attempt (crash, timeout) leaves the Linear issue's state alone (it
  may be In Review by now); only attempt 1 moves it to
  `linear.in_progress_state`. With `linear.blocked_state` set, every attempt
  moves it, to undo a block.

## Review (pull requests)

`task complete --pr https://github.com/<owner>/<repo>/pull/<n>` moves the task
to `in_review`, not `completed`. Its session is ended, its tmux window closed
and its worktree released (see [Cleanup](task-lifecycle.md#cleanup)); the branch `pq/<slug>`,
the agent session id and the PR URL are kept. An `in_review` task holds no
slot, so `max_concurrent` is free for the next task.

Every `scheduler.pr_poll_secs` (default 120) the daemon asks GitHub about the
PR with `gh api graphql` (state, mergeability, the head commit's checks, review
threads) and acts on the first rule that matches:

| The PR | powerqueue |
|--------|------------|
| merged | `completed`; local branch deleted. Linear is not touched (GitHub's integration moves the issue) |
| closed without merging | `needs_attention` |
| `CONFLICTING` | relaunch with reason `conflict` |
| a required check failed | relaunch with reason `ci_failed <check names>` (comma-separated) |
| dropped by the merge queue after the hand-off (a removal other than `merged` or `manual`) and neither queued nor armed again | relaunch with reason `ci_failed merge queue: <GitHub's reason>` |
| unresolved review threads with a comment newer than the hand-off | relaunch with reason `review` |
| labelled `scheduler.merge_hold_label` (`merge/hold`) | wait for a human to merge; status and dashboard say "waiting for manual merge" |
| unchanged for `scheduler.review_stale_hours` (24) | Linear comment + `needs_attention` |

A relaunch re-queues the task. When a slot and budget allow, the daemon
recreates the worktree at the **same path** from the kept branch (`git
worktree add <path> pq/<slug>`), fast-forwarded to `origin/pq/<slug>` after
the fetch so commits pushed to the PR since the hand-off are in it (a branch
with local commits the remote lacks is left as is), runs `repo.setup` and resumes the **same**
session (`claude --resume <session id>`) with `scheduler.review_prompt` as the
prompt — by default `/ship-pr <n> --reason <reason> <detail>`. The issue stays
in Linear's review state; only a comment says why the session was resumed.
Relaunches count against `scheduler.review_rounds_max` (5), not
`max_attempts`: each round gets its own `max_attempts` for crashes. Past the
cap the task goes to `needs_attention`; `task retry` runs one more round,
`task resume` just watches the PR again. `task retry` of a task that is
`in_review` resumes its session for a review round right away (reason
`requested`, detail `by user`; it does not count against
`review_rounds_max`), e.g. to talk to the agent about the PR. If the previous session cannot be
resumed (other provider, no transcript) a fresh session gets the full task
prompt with the review prompt as its first step.

The default review prompt assumes your agent has a `/ship-pr` command or skill.
If it does not, set `scheduler.review_prompt` to instructions your agent can
follow, using the [supported placeholders](configuration.md#scheduler).

`task show` lists the PR's timeline (`review.*` events: every status change
the watcher saw, relaunches, the merge); `status` and the dashboard count
`in review` apart from `running`. The watcher needs the GitHub CLI logged in
as the daemon's user (`gh auth login`); `doctor` checks it.

## Reading session memory

The dashboard's RSS column sums resident memory for the session process and
its child processes. Threads share their process's memory and count only once.
RSS is not virtual/reserved memory or whole-machine usage; separate processes
can still share pages, so their summed RSS need not match htop's host total.
Versions before 0.4.3 counted Linux threads repeatedly. Upgrade and restart the
daemon to correct live samples; previously stored peaks retain the old values.

## Responding to attention requests

Keep `powerqueue dashboard` open for a terminal bell when a task newly enters
`needs_attention` (sound/visual behavior depends on the terminal's bell settings).
Its footer shows the current number waiting; select a task and press Enter to
attach and answer. The notice disappears when no tasks need attention.

```sh
powerqueue task show ENG-123                 # reason and timeline
powerqueue task output ENG-123 -n 80         # inspect the actual prompt
powerqueue task send ENG-123 "Use the v2 endpoint, then finish."
powerqueue attach ENG-123                   # interact with permission dialogs
```

Use `task send` for text replies and `attach` for permission menus. The daemon
clears attention after it observes resumed activity, not merely when text is
sent. `task resume` remains available for explicitly resuming a task.

## Answering from Linear

For tasks that come from Linear you can answer on the issue itself (unless
`linear.post_comments = false`):

1. When the agent asks something — it runs `powerqueue task block`, prints the
   blocked marker, or ends its turn with a question — the daemon posts a
   comment on the issue: `🤖 Pregunta del agente`, the last paragraph of the
   agent's final message (plus the `task block` reason when it adds
   something) and a hidden `<!-- powerqueue:question -->` marker. Each
   question is posted once while it is open; asked again after a reply, it is
   posted again. `task block` counts only when the agent runs it from its own
   session (`POWERQUEUE_SESSION_ID`); a human's `task block` posts nothing.
2. On the Linear poll cadence (`linear.poll_interval_secs`) the daemon reads
   new comments of tasks that wait for a reply (`needs_attention`, or
   `in_review` with an unanswered question). The first comments newer than
   the question that powerqueue did not post are the answer: they are typed
   into the live session (as with `task send`), the task goes back to
   `running` and an issue moved to `linear.blocked_state` returns to
   `linear.in_progress_state`. If typing fails, the question stays open and
   the next poll tries again. When the session is gone (in review, parked) the task is
   re-queued and the next launch resumes the same session (`--resume`) with
   the answer as its prompt.
3. Comments on a `running` / `idle` task are typed into its session as a
   mid-flight hint.

Every comment powerqueue posts carries a hidden `<!-- powerqueue -->` marker
and its id is remembered, so the daemon never relays its own comments. A
comment the agent itself posts on the issue (e.g. through a Linear MCP server)
looks like a human one and is relayed as a hint; the hint says to ignore it
in that case. Replies are collapsed to one line before they are typed.
`doctor` lists the questions waiting for a reply and failed relays
(`relay.error`); events: `relay.question_posted`, `relay.answer_sent`,
`relay.answer_queued`, `relay.hint_sent`. Email replies are not consumed.

## Cleanup

On completion the daemon pushes the branch, removes the worktree, closes the
tmux window, and updates the source issue: Linear uses `linear.done_state`;
GitHub uses the optional `github.done_label` and `github.close_on_complete`.
Each integration controls summary comments independently. For GitHub tasks with
a PR, completion updates happen after the PR merges. Unpushed work is never deleted: if the push fails or there is
no remote, the worktree stays and an event says why. Failed tasks keep their
worktree when `cleanup.keep_failed` is true.

A task handed off for review (`task complete --pr`) is cleaned up the same way
when it enters `in_review` (`cleanup.run` commands, push, worktree removal),
except that the local branch is always kept for later review rounds, the tmux
window is always closed, and Linear is not moved. When the PR is merged the
local branch is deleted (and a worktree that had been kept is removed).
