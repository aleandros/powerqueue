# Configuration

[Documentation](README.md) · [Project overview](../README.md)

`powerqueue init` writes `config.toml` to the config directory (see
[Paths](operations.md#logs-and-diagnostics)). Defaults are shown below; `repo.path`
must point to your checkout, and enabled integrations need their required settings.
Unknown keys are rejected. `powerqueue config validate` and `doctor` explain
problems in plain language.

## Providers

powerqueue can run tasks on three locally installed coding-agent CLIs, each
paced against its own configured budget. The model names below are shipped
configuration defaults; access depends on your provider account and CLI.

| Provider | CLI | What it needs | Shipped models (most capable first) |
|----------|-----|---------------|-------------------------------------|
| `claude` (enabled by default) | Claude Code (`claude`) | authenticated CLI; check with `claude auth status` | `fable`, `opus`, `sonnet`, `haiku` |
| `codex` | OpenAI Codex CLI (`codex`) | authenticated CLI; check with `codex login status` | `gpt-6.1-sol`, `gpt-6-astra`, `gpt-6-luna` |
| `gemini` (experimental) | Google Antigravity CLI (`agy`) | authenticated CLI; integration has not been verified against a real `agy` installation | `gemini-3-pro`, `gemini-3-flash` |

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
gpt-6.1-sol`), else the first matching conditional `## Models` row (`if
label: model/fable: fable`), else the `## Models` entry for its criticality
(`critical: fable | gpt-6.1-sol`), each alternative through its provider's downgrade
chain. When nothing on the list is eligible, the first eligible model in
`budget.provider_order` (default `["claude", "codex", "gemini"]`) runs it, so
a provider that is out of budget or rate-limited falls back to the next one.
Model names infer their provider (`gpt-*` → codex, `gemini-*` → gemini);
anything else uses the explicit `codex:<name>` form.

## `[repo]`

| Key | Default | Meaning |
|-----|---------|---------|
| `path` | `""` (required) | main checkout; worktrees are created from it |
| `default_branch` | detect | base for new worktrees (`origin/HEAD`, then `main`/`master`) |
| `worktree_root` | `<data>/worktrees/<repo-name>` | where worktrees live |
| `branch_template` | `"pq/{key}"` | `{key}` = task key slug, `{id}` = short task id |
| `fetch_before_start` | `true` | `git fetch` before creating a worktree; new branches then start from `origin/<default_branch>` |
| `fast_forward_base` | `true` | after that fetch, before creating a branch, fast-forward the local default branch to `origin/<default_branch>` when it is safe (no local commits, no rebase/bisect, clean checkout; hooks off) |
| `setup` | `[]` | commands run (`sh -c`) in a fresh worktree before Claude starts, with `POWERQUEUE_TASK_ID`, `POWERQUEUE_TASK_KEY`, `POWERQUEUE_TASK_SLUG`, `POWERQUEUE_TASK_DIR`, `POWERQUEUE_BRANCH`, `POWERQUEUE_WORKTREE`, `POWERQUEUE_DATA_DIR` and `POWERQUEUE_STATE_DIR` in the environment |

## `[github]`

GitHub Issues and Linear can feed the same queue independently. Each daemon works
in one checkout: set `github.repository` to the `owner/repo` corresponding to
`repo.path`. For a GitHub-only installation, start with `powerqueue init --no-linear`
and add this to `config.toml`:

```toml
[github]
enabled = true
repository = "owner/repo"
required_labels = ["powerqueue"]
# Optional lifecycle labels; create these labels in the repository first:
# in_progress_label = "agent:working"
# done_label = "agent:review"
# blocked_label = "agent:blocked"
close_on_complete = false
```

Run `powerqueue secrets set github` (prompts for a token), then `powerqueue github test`
and `powerqueue github sync` to preview intake. Use `github sync --apply` to import
immediately, or let `powerqueue run` poll automatically. `GITHUB_TOKEN` overrides the
stored token. A fine-grained token needs access to the repository and **Issues: read**
for intake; comments, labels and closing need **Issues: write**. Issue intake
does not require the GitHub CLI; [PR watching](task-lifecycle.md#review-pull-requests) does.
API behavior follows the [GitHub Issues REST API](https://docs.github.com/en/rest/issues/issues).

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | `false` | enable daemon intake and lifecycle updates |
| `repository` | `""` | explicit `owner/repo`; required when enabled or using GitHub helpers |
| `required_labels` | `["powerqueue"]` | require **all** labels; `[]` accepts any open issue |
| `excluded_labels` | `["no-agent"]` | ignore issues with any listed label during intake |
| `assignee` | unset | GitHub login, `*` for assigned issues, `none` for unassigned; unset accepts any |
| `in_progress_label` | unset | lifecycle label added when an attempt starts |
| `done_label` | unset | lifecycle label added on successful completion |
| `blocked_label` | unset | lifecycle label added on blockage or permanent failure |
| `post_comments` | `true` | post start, completion and blocker/failure comments |
| `close_on_complete` | `false` | close the issue as completed when the task succeeds |
| `poll_interval_secs` | `60` | polling interval, 1–86400 seconds (minimum effective interval: 5 seconds) |
| `max_issues` | `100` | cap raw entries scanned per poll before PR/excluded-label filtering; locally finished tasks do not count; raise it for larger intake queues |
| `endpoint` | `https://api.github.com` | REST API base URL; supports Enterprise `/api/v3` and test servers |

Task keys are `owner/repo#123`; use quoted keys in commands, for example
`powerqueue task show 'owner/repo#123'`. Priority rules accept `source: github` and
issue labels. Linear-only priority, estimate, cycle, project and team fields remain
unset. Issue titles, bodies and labels populate the task and agent prompt.

Pull requests are excluded. Removing an intake label or changing the assignee does
not cancel an imported task. A confirmed closed issue cancels queued, throttled,
paused or crashed tasks; running sessions continue. API failures and missing access
(including HTTP 404) never count as closure. GitHub questions are posted when comments are enabled; answer them with `task send`
or `attach` (GitHub replies are not relayed). For tasks handed off with `--pr`,
the issue stays open during review and completion updates run when the PR merges.
Finished tasks are not re-imported;
use `task retry` to deliberately run one again. Lifecycle labels replace only other
configured lifecycle labels, preserving unrelated labels.

Polling backs off on failures and respects GitHub rate-limit reset headers. Lifecycle
writes are best effort: failures or writes skipped during backoff appear as
`github.update_failed` / `github.update_skipped` events; check `task show` / `logs`
and update the issue manually. `doctor` checks credentials and repository/Issues
read access (write permissions cannot be verified without making a change).
`run --offline` disables both remote sources. Explicit `github` helpers work even
when `github.enabled = false`, so you can preview configuration before enabling it.

## `[linear]`

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | `true` | poll Linear at all |
| `team_keys` | `[]` | team keys to pull from; empty = every team the key can see |
| `assignee` | none | only issues assigned to this user id, or `"me"` |
| `queued_states` | `["Todo"]` | workflow state names that mean "ready for the agent" |
| `required_labels` | `[]` | issues must carry one of these labels |
| `excluded_labels` | `["no-agent"]` | issues with any of these are ignored |
| `cycle` | `"any"` | which cycles to pull from, filtered server-side: `any`, `active` (alias `current`), `next`, `active-or-next`, or `none` (issues outside any cycle). The issue's cycle is also exposed to `PRIORITY.md` as `cycle` / `cycle_number` |
| `projects` | `[]` | only issues in these projects (matched by project name, server-side); empty = any project |
| `in_progress_state` | `"In Progress"` | state set when a session starts; `""` = leave the state alone for this transition |
| `done_state` | `"In Review"` | state set on completion; `""` = no change |
| `blocked_state` | none | state set when a task fails permanently or is blocked |
| `done_state_parent` | `"Done"` | state a parent issue (one with sub-issues) is moved to once every sub-issue is completed or canceled (at least one completed); a comment lists the sub-issues. `""` = no change. See [Dependencies](configuration.md#dependencies-blocked-by-and-parent-issues) |
| `manage_states` | `true` | let powerqueue move issues between workflow states at all. Set it to `false` when your own Claude skills or CI move issues: the daemon then never changes an issue's state, while comments still follow `post_comments` |
| `post_comments` | `true` | which comments to post on the issue: `true` (progress comments plus everything below), `"questions"` (only agent questions, parent auto-close and PR merge/hold/review-round notices) or `false` (none). Anything but `false` also relays replies on the issue back to the session (see [Answering from Linear](task-lifecycle.md#answering-from-linear)) |
| `poll_interval_secs` | `60` | how often to poll |
| `max_issues` | `100` | cap per poll |
| `endpoint` | `https://api.linear.app/graphql` | GraphQL endpoint (tests) |

### Dependencies: `blocked by` and parent issues

Each poll also reads the issue's Linear relations:

* **Blocked by.** A task whose issue is `blocked by` another one is moved to
  the `blocked` state and never started until every blocker is satisfied:
  its Linear state is of type `completed` or `canceled`, **or** the GitHub
  pull request attached to it (Linear's GitHub integration) is merged. It
  then goes back to `queued` on its own. `status` and the dashboard show a
  **WAITING ON** column with the pending keys, `task show` a `waiting on`
  line, and `task explain` lists every blocker and why it is (not) satisfied. Only
  `queued`/`throttled` tasks are moved; a task that already ran is never
  pulled back, but a crashed one is not relaunched while it waits.
* **Parent issues.** An issue with sub-issues is a container: it is synced
  (as `blocked`) but never run. powerqueue watches every parent it sees (the
  parent of any synced sub-issue, even one that is not in `queued_states`
  itself), and once all its sub-issues are closed with at least one
  completed it moves the parent to `done_state_parent` (when
  `manage_states` is on), posts a comment listing the sub-issues (when
  `post_comments` is on) and completes the parent's task, if it has one
  (cleaning up its worktree if it had run before getting sub-issues).
  The daemon looks watched parents up every 5 minutes, so closing one can
  lag its last sub-issue by that much. A parent whose
  sub-issues were all canceled is left alone (`status` shows
  `parent (all canceled)`, `doctor` warns).

Relations and sub-issues are read with a separate query in batches of 10
issues, following every page, so long lists are complete. If that query
fails the whole poll is skipped rather than risk starting a blocked task.
Once a blocker's PR is seen merged it stays satisfied; an unmerged one is
asked again at most every 5 minutes.

`powerqueue doctor` reports blocked tasks, `blocked by` cycles between open
tasks (which would wait forever) and failed attempts to close a parent.

## `[priority]` and `[priority.jev]`

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

## `[prompt]`

| Key | Default | Meaning |
|-----|---------|---------|
| `template` | none | path to a Markdown template for the first message of every session; `~` is expanded, a relative path resolves against the config directory. Unset = the built-in prompt |
| `instructions` | none | extra instructions appended to every prompt under a `## Instructions` heading (also `{{instructions}}` in templates) |

A template is plain Markdown with `{{placeholders}}`. Unknown placeholders
are left as written and reported once per launch as a `prompt.template_error`
event; a template that cannot be read is reported the same way and the
built-in prompt is used, so a typo never blocks scheduling. A repository can
point to its own template with `prompt_template` in `.powerqueue.toml`
(relative to the repo root; it wins over the global one). Preview the result
for any task with `powerqueue task prompt <task>` and start from
[docs/examples/prompt-template.md](examples/prompt-template.md), which
reproduces the built-in prompt.

| Placeholder | Value |
|-------------|-------|
| `{{key}}`, `{{title}}`, `{{description}}` | task key, title and (trimmed) description |
| `{{url}}`, `{{source}}`, `{{team}}` | issue URL (empty for manual tasks), `linear`, `github` or `manual`, Linear team key |
| `{{labels}}`, `{{project}}`, `{{priority}}`, `{{estimate}}`, `{{cycle}}`, `{{cycle_number}}` | labels (comma-joined), project name, priority word (`urgent`, `high`, ...), estimate, cycle status (`active`, `next`, `past`, `future`) and number; empty when unknown |
| `{{branch}}`, `{{worktree}}` | the branch and worktree the session works in |
| `{{task_id}}`, `{{attempt}}`, `{{max_attempts}}`, `{{previous_error}}` | task id (as used by `powerqueue task complete`), attempt number, attempt cap, how the previous attempt ended (empty on the first) |
| `{{model}}`, `{{provider}}` | the model and provider of this session |
| `{{working_rules}}` | the `## Working rules` block (branch rule, commit rule for the provider's sandbox, no pushing) |
| `{{completion_protocol}}` | the `## Completion protocol` block with the `powerqueue task complete <id>` / `task block <id>` commands and the `[[POWERQUEUE:DONE]]` / `[[POWERQUEUE:BLOCKED]]` markers |
| `{{powerqueue}}` | the `powerqueue` command the session should run: `powerqueue`, or the task's shim path with `<provider>.shim` (see [Running sessions in containers](containers.md)) |
| `{{attempt_notes}}` | the `## Attempt N` block; empty on the first attempt |
| `{{instructions}}` | `prompt.instructions` |
| `{{default_prompt}}` | the whole built-in prompt, so a template can wrap it |

Keep `{{completion_protocol}}` (or its commands and markers) in your template:
without it the session has no way to tell powerqueue it is done.

## `[scheduler]`

| Key | Default | Meaning |
|-----|---------|---------|
| `max_concurrent` | `2` | parallel sessions |
| `max_attempts` | `3` | attempts before a task is `failed` |
| `tick_secs` | `5` | main loop period |
| `idle_timeout_secs` | `600` | silent after a turn without a marker: nudge once, then `needs_attention` |
| `stale_session_secs` | `1800` | no transcript growth or hook events while running = hung |
| `restart_backoff_secs` | `[30, 120, 600]` | backoff after a crash, per attempt (last value repeats) |
| `max_session_secs` | `14400` | cap on the agent's working time per attempt (time spent `needs_attention`, paused or throttled does not count); `0` disables |
| `resource_sample_secs` | `30` | CPU/RSS sampling interval |
| `pr_poll_secs` | `120` | how often the PR of each `in_review` task is checked with `gh`; `0` turns the watcher off |
| `review_rounds_max` | `5` | relaunches of one task for its PR (conflict, failed check, review) before `needs_attention`; separate from `max_attempts` |
| `review_stale_hours` | `24` | a PR unchanged this long gets a Linear comment and the task goes to `needs_attention`; `0` disables |
| `merge_hold_label` | `"merge/hold"` | PR label meaning "a human merges this": while the PR carries it, the watcher waits and never reports it stale |
| `review_prompt` | `"/ship-pr {pr} --reason {reason} {detail}"` | prompt of a resumed review session; placeholders `{pr}`, `{url}`, `{reason}` (`conflict`, `ci_failed`, `review`, `requested` for `task retry`), `{detail}` |
| `gh_binary` | `"gh"` | GitHub CLI the watcher runs |

## `[claude]`

| Key | Default | Meaning |
|-----|---------|---------|
| `binary` | `"claude"` | the command that runs Claude Code: a program, or a command line with leading arguments and per-task placeholders (`{key}`, `{slug}`, `{task_id}`, `{session_id}`, `{worktree}`, `{task_dir}`, `{repo}`, `{attempt}`, `{model}`), e.g. `"docker exec -it --env-file {task_dir}/env -w {worktree} pq-{slug} claude"`; see [Running sessions in containers](containers.md) |
| `shim` | `false` | the session runs where the daemon's `powerqueue` binary cannot (a container): hooks, the status line and `powerqueue task complete\|block` go through a shell shim at `<task dir>/bin/powerqueue` and the task inbox the daemon drains |
| `permission_mode` | `"acceptEdits"` | `--permission-mode`; `auto` is the unattended choice, `bypassPermissions` never asks at all |
| `effort` | none | `--effort` value |
| `extra_args` | `[]` | flags appended verbatim |
| `allowed_tools` | `[]` | extra `--allowedTools` patterns |
| `append_system_prompt` | none | appended to the system prompt for every task |
| `fallback_models` | `[]` | passed as `--fallback-model` |
| `trust_workspace` | `true` | mark the repository and each worktree as trusted in Claude Code's `~/.claude.json` before launching, so sessions never wait on the workspace-trust dialog |
| `env` | `{}` | environment variables for the session; `CLAUDE_CONFIG_DIR` here is also where the daemon reads transcripts and seeds workspace trust |

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

## `[codex]`

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
task complete`) with `--add-dir`. Codex keeps every `.git` directory read-only
in all sandboxed modes (verified with `codex sandbox`: even `git commit` in a
plain repository fails with "Operation not permitted"), so in `workspace-write`,
`approve-for-me` and `on-request` the prompt tells the session **not** to
commit; the changes stay in the working tree and cleanup commits them
(`cleanup.commit_uncommitted`, message `powerqueue: uncommitted changes from
<key>`) before pushing. Only `yolo` sessions commit themselves. The
`[[POWERQUEUE:DONE]]` marker in the final message completes the task even if
the completion command fails.

| Key | Default | Meaning |
|-----|---------|---------|
| `binary` | `"codex"` | the command that runs Codex: a program or a command line with per-task placeholders (same rules as `claude.binary`) |
| `shim` | `false` | route `notify` and `powerqueue task complete\|block` through the task inbox (same meaning as `claude.shim`) |
| `approval` | `"workspace-write"` | `workspace-write` (`-a never -s workspace-write`), `approve-for-me` (`--approve-for-me`), `yolo` (`--dangerously-bypass-approvals-and-sandbox`) or `on-request` (`-a on-request -s workspace-write`) |
| `reasoning_effort` | `"high"` | `-c model_reasoning_effort=…` (`low`, `medium`, `high`, `xhigh`, `max`, `ultra`); unset to leave Codex's default |
| `extra_args` | `[]` | flags appended verbatim |
| `env` | `{}` | environment variables for the session (`CODEX_HOME` here is also where the daemon looks for rollouts) |
| `trust_workspace` | `true` | mark the worktree as a trusted project for the session (`-c projects={...}`, an inline table because Codex splits dotted `-c` keys on `.`) so Codex never stops at its folder-trust dialog |

## `[gemini]` (experimental)

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
| `binary` | `"agy"` | the command that runs the CLI: a program or a command line with per-task placeholders (same rules as `claude.binary`) |
| `shim` | `false` | route the hook and `powerqueue task complete\|block` through the task inbox (same meaning as `claude.shim`) |
| `mode` | `"skip-permissions"` | `skip-permissions` (`--dangerously-skip-permissions`), `accept-edits` or `plan` (`--mode`) |
| `effort` | `"high"` | `--effort` (`low`, `medium`, `high`); unset to leave the default |
| `extra_args` | `[]` | flags appended verbatim |
| `env` | `{}` | environment variables for the session |

## `[budget]` and `[budget.providers.<provider>]`

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

Usage probes read allowance information: Codex through `codex app-server`
(`account/rateLimits/read`), Claude through the status line every task's
`settings.json` configures (`powerqueue hook … --event StatusLine`, which
also shows `pq <model> 5h 75% · 7d 89%` in the session), Antigravity through
`agy -p /usage` (experimental). The daemon runs them at start and every
`probe_interval_mins` in the background; `budget probe` runs them now. Claude's
probe reads the last stored status-line observation; it does not request a
fresh reading from Claude. The
reported weekly reset becomes the period anchor, the reported usage is where
pacing starts from (and, over time, teaches the exchange rate between
powerqueue's weighted tokens and the provider's percentages), and a provider
that reports its allowance exhausted is skipped until the reset. Details: [docs/budget.md](budget.md#usage-probes-and-observed-anchors).

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

## `[cleanup]`

| Key | Default | Meaning |
|-----|---------|---------|
| `remove_worktree` | `true` | remove the worktree after completion |
| `push_branch` | `true` | push before removal so work is not lost |
| `delete_branch` | `false` | delete the local branch afterwards |
| `keep_failed` | `true` | keep worktrees of failed tasks |
| `run` | `[]` | commands run (`sh -c`) in the worktree before removal |
| `close_tmux_window` | `true` | kill the window; otherwise it stays with the final output |
| `commit_uncommitted` | `true` | commit changes the session left uncommitted (`powerqueue: uncommitted changes from <key>`) before pushing or removing the worktree; a failure keeps the worktree |

`cleanup.run` commands execute with `sh -c` inside the worktree, before the
auto-commit and the push, with `POWERQUEUE_TASK_ID`, `POWERQUEUE_TASK_KEY`,
`POWERQUEUE_TASK_SLUG`, `POWERQUEUE_TASK_DIR`, `POWERQUEUE_BRANCH`,
`POWERQUEUE_WORKTREE`, `POWERQUEUE_DATA_DIR`, `POWERQUEUE_STATE_DIR` and
`POWERQUEUE_SUCCEEDED` (`1`/`0`) in the environment. A failing command is logged
(`cleanup.command_failed`) and the worktree is kept. That is also the hook
for an agent-driven teardown, e.g. a one-shot session that tidies up what
the task left behind:

```toml
[cleanup]
run = ["[ \"$POWERQUEUE_SUCCEEDED\" = 1 ] && claude -p --permission-mode acceptEdits \"$(cat .powerqueue/cleanup-prompt.md)\" || true"]
```

## `[tmux]`

| Key | Default | Meaning |
|-----|---------|---------|
| `binary` | `"tmux"` | tmux executable |
| `session_name` | `"powerqueue"` | session hosting one window per task |
| `socket_name` | none | tmux `-L` socket name |
| `remain_on_exit` | `true` | keep dead panes so crashes can be inspected |

## `[logging]`

| Key | Default | Meaning |
|-----|---------|---------|
| `level` | `"info"` | `tracing` filter for the log file, e.g. `info,powerqueue=debug` |
| `keep_days` | `14` | rotated daily files to keep |
| `json` | `true` | JSON lines (true) or text (false) in the file |

## `[tune]`

`powerqueue tune` runs a headless Claude Code session (`claude -p`) that edits
drafts of `PRIORITY.md` and `config.toml`; see [Tuning with
`tune`](priority.md#tuning-with-powerqueue-tune).

| Key | Default | Meaning |
|-----|---------|---------|
| `model` | `"sonnet"` | Claude model alias for the tuning session (`-m` overrides per run; must be a Claude model) |
| `timeout_secs` | `600` | kill the session after this long; the draft is kept and reported as failed |
| `extra_args` | `[]` | flags appended to the `claude -p` command line |
| `keep_drafts` | `20` | drafts to keep under `<state>/tune/`; older finished ones (applied, undone, unchanged, failed, invalid) are pruned after each run, proposed ones never |

The session uses `claude.binary` and refuses commands with per-task placeholders.
It runs in the draft directory with
`--permission-mode acceptEdits`, and may only call `powerqueue priority
check --file`, `powerqueue priority simulate --file --config` and
`powerqueue config validate --file` (all read-only against the drafts).

## Running sessions in containers

Sessions can run inside a per-task container (or anywhere the daemon's own
`powerqueue` binary cannot run) with everything else unchanged: the tmux pane
is the CLI's terminal, `attach` and `task send` work, crashes resume the same
session, transcripts are tailed, `task complete` ends the task. Three
settings do it, and every path involved must be mounted at the same absolute
path inside the container:

```toml
[repo]
setup = ["tools/agent-container.sh up"]        # docker run -d --name pq-$POWERQUEUE_TASK_SLUG ...
[cleanup]
run = ["tools/agent-container.sh down"]        # docker rm -f pq-$POWERQUEUE_TASK_SLUG

[claude]
binary = "docker exec -it --env-file {task_dir}/env -w {worktree} pq-{slug} claude"
shim = true
[claude.env]
CLAUDE_CONFIG_DIR = "/home/me/.local/share/powerqueue/agent-home/claude"
```

`binary` is a command template (shell-style splitting, per-task
placeholders), `shim = true` gives the session a shell shim at `<task
dir>/bin/powerqueue` whose `hook` and `task complete|block` calls land in
`<task dir>/inbox/` for the daemon to drain (`inbox.complete`, `inbox.block`,
`inbox.rejected` events; `doctor` reports parked messages), and
`CLAUDE_CONFIG_DIR` in the session env is where the daemon reads transcripts
(mount it too; the subscription login lives there). The same works for
`[codex]` with `CODEX_HOME`. [docs/containers.md](containers.md) has the
full recipe, an example image
([docs/examples/Dockerfile.agent](examples/Dockerfile.agent)) and the
`up` / `down` / `exec` helper
([docs/examples/agent-container.sh](examples/agent-container.sh)).

## Per-repository overrides: `.powerqueue.toml`

A `.powerqueue.toml` in the repository root overrides a subset of the global
config for that repo. Lists replace; `instructions` and
`claude.append_system_prompt` are appended to the global value;
`prompt_template` (relative to the repo root) replaces `prompt.template`.

```toml
setup = ["npm ci"]
default_branch = "develop"
branch_template = "agent/{key}"
instructions = "Run `npm test` before you finish. Never touch migrations."
prompt_template = "docs/agent-prompt.md"   # see [prompt] above

[cleanup]
remove_worktree = false     # also: push_branch, delete_branch, keep_failed, run, close_tmux_window

[claude]
permission_mode = "plan"    # also: effort, extra_args, allowed_tools, append_system_prompt

[codex]
approval = "on-request"     # also: reasoning_effort, extra_args

[gemini]
mode = "accept-edits"       # also: effort, extra_args
```
