# Configuration

[Documentation](README.md) · [Project overview](../README.md)

`powerqueue init` writes `config.toml` to the config directory (see
[Paths](operations.md#logs-and-diagnostics)). Defaults are shown below; `repo.path`
must point to your checkout, and enabled integrations need their required settings.
Unknown keys are rejected. `powerqueue config validate` and `doctor` explain
problems in plain language.

## Execution projects

Use `project register ID PATH --github OWNER/REPO` for focused setup and
reconfiguration, and `project list --json` to inspect effective settings. See the
[multi-project walkthrough and smoke test](multi-project.md).

One daemon can schedule tasks across several checkouts on the same machine.
The registry keys are stable execution project IDs, independent of Linear's
project names and of GitHub issue repositories. Issue admission and source
metadata remain in `[linear]` and `[github]`; priority decides ordering and
model preferences, while execution settings decide how work runs.

Keep your existing `[repo]` as the `default` project and add repositories:

```toml
[repo]
path = "/home/me/code/app"

[scheduler]
max_concurrent = 3 # shared by every project

[projects.api.repo]
path = "/home/me/code/api"
setup = ["cargo build"]

[projects.api.prompt]
instructions = "Run the API contract checks before finishing."

[projects.api.cleanup]
push_branch = true

[projects.api.priority]
file = "/home/me/code/api/priority.toml"
```

Use `powerqueue add "Fix the API" --project api`. IDs allow ASCII letters,
digits, `_` and `-`. Checkout paths in the registry must be absolute (`~` is
expanded). All `RepoConfig` keys from the `[repo]` table below are available in
`[projects.<id>.repo]`, with the same defaults. Optional `prompt`, `cleanup`, and
`priority` tables use the same keys and defaults as their global counterparts.
If supplied, these tables replace their global table, then the checkout's
`.powerqueue.toml` overrides are applied. Relative prompt template paths in
config.toml resolve from the config directory; paths in `.powerqueue.toml`
resolve from that project's checkout, as in single-repository mode.

Without a named project table, the global setting is inherited. Repository-local
setup, agent options, prompts, priority file, cleanup and task/review limits
apply only to that project. With a registry configured, named repositories'
source admission (`linear`/`github`) overrides are ignored. The legacy `[repo]`
checkout's source overrides remain machine-wide, preserving existing approval
restrictions and intake filters when projects are added. Its source overrides
continue to reload; `config show` identifies the applied keys. Repository
overrides for total `max_concurrent`, `tick_secs`, and `resource_sample_secs`
are ignored in registry mode. Provider budgets, cooldowns and usage
ledgers are always machine-wide; projects cannot declare a budget or scheduler
table in config.toml. One launch pass allocates slots and reserves predicted
usage across all projects in priority order.

The legacy `default` keeps its worktree paths. Explicit registry entries use
`<data>/worktrees/projects/<id>/<task-slug>`, or `<worktree_root>/<id>/<task-slug>`
when that project sets a custom root. Session files remain keyed by globally
unique task IDs. Setup, cleanup and agent environments include
`POWERQUEUE_PROJECT`. Never change a stable ID to refer to a different checkout.

**Migration:** existing TOML files need no edits; old database rows and task
JSON acquire `execution_project = "default"`. The database migration is
idempotent. Keep the original checkout available for the first upgraded run:
existing tasks with execution history are pinned to its canonical path. A task
is otherwise pinned before its first repository operation. Retry, daemon
restart and PR review preserve that binding and existing worktree paths.
You may use `[projects.default.repo]` instead of `[repo]`, but cannot configure
both. A named-only registry works with explicit source routing. Register `default`
for `add` without `--project`; Linear can use `linear.execution_project` or routing
rules. Dashboard additions use the selected `--project`, otherwise `default`; a missing selection fails. GitHub sources
select their configured `execution_project` (also `default` when omitted).

`powerqueue task project <task> <id>` deliberately reassigns an unstarted task,
validates the target, and logs `task.project_set`. It refuses a task with a
previous binding, session, branch, worktree or review history, even after retry.
To move work after execution started, cancel/finish the old task and create a
new task with `add --project`; existing branches and transcripts stay associated
with their original task. Removing or moving a registered checkout prevents its
bound tasks from launching or cleaning up; restore it rather than editing the
database. Existing worktrees are checked for repository ownership before use.

Project overrides are read each tick; rules reload each tick when
`priority.live_reload` is true (otherwise on restart or configuration reload).
Invalid files suspend launches
for that project and log `project.error`. Committed overrides are refreshed by
that project's `fetch_interval_secs` in the background. Restart or request a
configuration reload after editing the registry itself. `config validate`
checks the registry and overrides; `doctor` checks every checkout, worktree root,
policy file and stored task binding. `task show` and its JSON expose both the
execution project and pinned checkout. A broken project does not stop other
projects from using the shared queue.

## Queues and workflows

See [triage queues and rollout](triage.md) for a complete same-repository example.
All new settings are opt-in; empty queue/workflow tables preserve existing behavior.

| Key | Default | Meaning |
| --- | --- | --- |
| `queues.<id>.max_concurrent` | `1` | Dedicated slots; sum must fit `scheduler.max_concurrent`. `default` is required when queues are configured. |
| `queues.<id>.all` | `[]` | Priority-style selection predicates. Required for named queues; forbidden for `default`, which receives unmatched work. Conflicting matches hold the task. |
| `queues.<id>.workflow` | unset | Selected workflow; unset retains existing coding prompts and lifecycle. |
| `workflows.<id>.instructions` | `""` (required) | Markdown path relative to the task worktree. Missing/empty files or paths leaving the checkout fail the launch. |
| `workflows.<id>.completion` | `"standard"` | `leave-ticket-open` completes analysis without source state changes, PR adoption, or automatic commit/push. |
| `workflows.<id>.comment_prefix` | `""` | First text of workflow comments; e.g. `[interno]` for an integration's internal notes. |
| `linear.team_filters.<TEAM>.assignee` | unset | Independent team intake: any assignee unless supplied. |
| `linear.team_filters.<TEAM>.queued_states` | `["Todo"]` | State names admitted for this team. |
| `linear.team_filters.<TEAM>.required_labels` | `[]` | At least one required label, or no requirement. |
| `linear.team_filters.<TEAM>.excluded_labels` | `["no-agent"]` | Labels that exclude this team's issues. |
| `linear.team_filters.<TEAM>.cycle` | `"any"` | Same cycle scopes as `[linear]`. |
| `linear.team_filters.<TEAM>.projects` | `[]` | Any Linear project unless listed. |
| `linear.team_filters.<TEAM>.max_issues` | `100` | Per-filter issue cap; `0` means unlimited. |

Queue/workflow tables live in host configuration. Team intake filters may also
live in the legacy repository's `[linear]` override. Team filters replace the
base intake for their team, even when it was absent from `linear.team_keys`.
Queue selection is independent of execution-project routing, uses the existing
priority predicate grammar, and is pinned before the first launch. Existing
started tasks retain the default pool. Retries preserve the binding. Slots are
not borrowed; shared provider budget limits still apply.

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
`[[overrides]]` models in `priority.toml`, else the first matching
`[[model_rules]]` row, else `[models]` for its criticality
(e.g. `critical = ["fable", "gpt-6.1-sol"]`), each alternative through its
provider's downgrade chain. When nothing on the list is eligible, the first eligible model in
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
| `setup` | `[]` | background commands run sequentially (`sh -c`) in a fresh worktree before Claude starts, with `POWERQUEUE_TASK_ID`, `POWERQUEUE_TASK_KEY`, `POWERQUEUE_TASK_SLUG`, `POWERQUEUE_TASK_DIR`, `POWERQUEUE_BRANCH`, `POWERQUEUE_WORKTREE`, `POWERQUEUE_DATA_DIR` and `POWERQUEUE_STATE_DIR` in the environment |
| `overrides_from` | `"working-tree"` | where the repository's [`.powerqueue.toml`](#per-repository-overrides-powerqueuetoml) (and a relative `priority_file` named in it) is read from: the main checkout's working tree, or `"default-branch"` for the file as committed on `origin/<default_branch>` (the local branch when there is no remote), so a `git checkout` in the main checkout cannot change what the daemon runs |
| `fetch_interval_secs` | `300` | with `overrides_from = "default-branch"`, how often the daemon runs `git fetch` so a change merged upstream reaches it while the queue is idle; `0` = only the fetch before a task starts; at most a week (604800) |

`repo.setup` runs outside the daemon tick. The task remains `starting` and
reserves a global slot, its queue slot and predicted model budget until setup
finishes. Heartbeats, source sync, commands and other available slots continue.
`status` shows setup elapsed time; `status --json` includes `setup_started_at`.
Setup errors use normal crash backoff and attempt limits. Commands run in order
and stop at the first failure. No setup timeout is imposed; bound long commands
in your script when needed. Pausing or cancelling the task prevents its agent
from launching; cancellation cleanup waits for setup to exit. Scheduling pause
holds completed setups until scheduling resumes. Stopping the daemon does not
wait for setup: surviving shell children retain their lease, and the next daemon
waits for them before recovering the abandoned attempt. `run --once` performs
one admission pass, then keeps heartbeating and supervising its pending setups
through launch or failure before exiting. It admits no new tasks during that drain.

## `[github]`

GitHub Issues and Linear can feed the same queue independently. Each GitHub
source maps an `owner/repo` to a registered execution project. The source repository
need not be the execution checkout. For a GitHub-only installation, start with `powerqueue init --no-linear`
and add this to `config.toml`:

```toml
[github]
enabled = true
repository = "owner/repo"
required_labels = ["agent:ready"]
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
Intake uses the [GitHub Issues REST API](https://docs.github.com/en/rest/issues/issues); approval verification also needs [GraphQL issue history](https://docs.github.com/en/graphql/reference/issues). The GraphQL endpoint is derived from `endpoint`: `/graphql`, or `/api/graphql` for an Enterprise `/api/v3` base.

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | `false` | enable daemon intake and lifecycle updates |
| `repository` | `""` | explicit source `owner/repo`; required when enabled or using GitHub helpers |
| `execution_project` | `"default"` | fallback registered execution project ID; matching routing rules override it; `""` requires a rule match |
| `token_env` | unset | source-specific token environment variable; if set, missing/empty values fail without falling back to the global token |
| `required_labels` | `["agent:ready"]` | configurable execution approval labels; **all** require current label-application events; cannot be empty when enabled |
| `trusted_approvers` | `[]` | optional GitHub login allowlist (case insensitive); every effective approval event must be by an allowed actor; empty allows any verifiable actor |
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

### GitHub dependencies

Use GitHub's **Relationships → Mark as blocked by** on the dependent issue.
Both issues may carry `agent:ready`: powerqueue keeps the dependent task `blocked`
until every prerequisite is closed in GitHub or its blocking relationship is
removed. Reopening a prerequisite holds future starts again. A merged PR satisfies
this gate when it closes its prerequisite issue (for example with `Closes #123`
or `close_on_complete = true`); an open issue with only a linked/merged PR remains
blocking. No issue-body syntax or `blocked` label is interpreted as a dependency.

Native prerequisites may be in another repository and do not need to be in this
queue or approved for execution. The source token must be able to read them.
Every page is checked against GitHub's dependency counts. Missing summaries,
hidden/inaccessible prerequisites, malformed responses, unsupported GitHub
Enterprise dependency APIs and partial pagination hold execution; they never
mean that a prerequisite is done. `task show`, `task explain` and `doctor` report
waiting keys or the verification error. Fix access and let the next poll retry;
removing/reapplying approval is not needed for a dependency-only change.

Dependencies are refreshed during source polling and immediately before every
new start, crash restart and review round, both before and after worktree setup.
Changes during setup preserve the worktree and release the reserved slot.
Active sessions continue their current work; pause/cancel them explicitly if
needed. Manual pauses remain paused after blockers clear. Analysis workflows
with `completion = "leave-ticket-open"` retain their existing exemption from
dependency waits. Remote changes after the final API response cannot be atomic
with launching a local process.

### Multiple GitHub sources

Keep `[github]` for the original source and add `[[github_sources]]` tables, or
configure all sources in that array (default `[]`). Each entry supports every
key above, with independent defaults, filters, approval rules, credentials,
polling interval and lifecycle settings. Enable each source explicitly:

```toml
[github]
enabled = true
repository = "acme/web-issues"
execution_project = "default"

[[github_sources]]
enabled = true
repository = "acme/api-issues"
execution_project = "api" # register [projects.api.repo] as shown above
required_labels = ["agent:ready", "team:api"]
trusted_approvers = ["api-maintainer"]
token_env = "POWERQUEUE_API_GITHUB_TOKEN"
done_label = "agent:review"
post_comments = false
```

`token_env` stores only the variable name in TOML. Make that variable available
in the daemon's environment as well as your interactive shell. Omit it to use
`GITHUB_TOKEN` or the existing stored GitHub token. A source with read access may
import while another source uses write access for comments, labels and closure.
Failed writes are reported and need manual reconciliation. Failed credentials,
access, polling and rate limits affect only that source; a missing issue or failed
request never counts as confirmed closure in any repository.

Repositories are matched case insensitively. Configure each `owner/repo` only
once, including across API hosts: task identity is `owner/repo#number`, so the
same identity on two hosts is rejected rather than merged. Duplicate mappings
and unregistered project IDs are configuration errors. Different repositories
can have the same issue number, and several sources can target the same project.

`github test` and `github sync` visit every configured source, including disabled
sources for explicit previews. Successful sources continue when another fails;
errors identify the repository and the command exits nonzero. `sync --apply` may
therefore import successful sources while reporting errors for others. JSON keeps
`fetched` and `changes`, and adds `sources` and `errors`; preview rows show the
saved execution project. `task show` exposes source identity, saved project and
admission, and `doctor` reports each source's access and routing conflicts.

**Migration:** existing `[github]` configurations and stored tokens need no
changes. Omitted `execution_project` means `default`; existing task assignments
are preserved. Routing applies when a task is created. Later mapping changes do
not retarget queued, running, retried or review tasks. A mismatch blocks future
launches and appears in routing diagnostics. Restore the mapping or deliberately
reassign unstarted tasks using `task project`; started work stays bound to its
original project. Source lifecycle writes still target the original issue when
that source remains configured and enabled. Removing or disabling a source blocks
launches and reports skipped lifecycle writes.

Manual `add --project ID` validates registration. Omitting `--project` always
selects `default`, even with multiple projects; it never guesses from the current
directory or source metadata. An unknown ID fails without creating a task.
Linear uses `linear.execution_project` (default `"default"`) when no routing rule matches.

Task keys are `owner/repo#123`; use quoted keys in commands, for example
`powerqueue task show 'owner/repo#123'`. Priority rules accept `source: github` and
issue labels. Linear-only priority, estimate, cycle, project and team fields remain
unset. Issue titles, bodies and labels populate the task and agent prompt.

Planning, triage and execution are separate steps. Planning issues describe possible
work; triage labels such as `bug`, `enhancement` or `planning` classify it. Neither
approves execution. Apply an execution approval label deliberately after reviewing
the requested work. Public issue forms and automated categorization must not assign
approval labels indiscriminately. The label name is configurable: for example,
`required_labels = ["team:approved"]`. If multiple labels are configured, **each**
is an approval label with its own effective approval event. Lifecycle labels must
be distinct from approval labels so the daemon cannot grant itself approval.

To restrict who can approve, set `trusted_approvers = ["edgar", "maintainer"]`
globally or in `.powerqueue.toml`. Admission checks the actor on the latest effective
application of each label, never the issue author. Removing and reapplying a label
replaces the earlier approval, so an untrusted reapplication does not inherit a
trusted approval. Missing actors/events, incomplete history, GraphQL errors or
unavailable credentials prevent launch, including after daemon restart.

Any title or body edit invalidates earlier approval, including formatting edits or
edits later reverted. Review the changed issue, then remove and reapply every
approval label. Edits in the same timestamp second as approval are treated
conservatively: reapply in a later second. Comments, unrelated labels, assignments
and priority changes do not invalidate approval by themselves; excluded labels and
the configured assignee filter still prevent execution. Priority rules and local
`task resume`, `retry` or model changes cannot bypass admission.

Approval is reconciled during polling and freshly verified before every start,
crash restart and review round, with another check after worktree setup. A launch
revoked during setup is held (`github.launch_held`) with its artifacts preserved.
Revoked tasks retain their queue state and worktree,
but cannot launch; reapproval restores eligibility without overriding a manual pause.
An already active session may finish its existing work: revocation does not kill
its pane, remove its worktree or cancel the task. Stop active work explicitly with
the task controls when needed. `task show`, `task explain` (also `--json`), sync
previews and `doctor` explain decisions; `github.admitted`, `github.revoked` and
`github.admission_blocked` events retain the approval actor/event or denial reason.

**Migration:** new configurations and omitted `required_labels` now default to
`["agent:ready"]`. Explicit settings such as `["powerqueue"]` are preserved and
that label becomes the deliberate execution approval label. To adopt the new name,
create `agent:ready`, update `required_labels`, review queued issues and apply the
new label deliberately. If the previous `powerqueue` label was applied automatically,
review the issue and remove/reapply it (or migrate to a fresh label). Previously
imported tasks start unverified on upgrade and must pass fresh history checks;
`required_labels = []` is no longer valid for enabled GitHub intake.

Pull requests are excluded. A confirmed closed issue cancels queued, throttled,
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
`run --offline` disables both remote sources and prevents GitHub tasks from starting without online approval verification. Explicit `github` helpers work even
when `github.enabled = false`, so you can preview configuration before enabling it.

## `[linear]`

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | `true` | poll Linear at all |
| `execution_project` | `"default"` | registered fallback execution project; `""` requires a routing rule match |
| `team_keys` | `[]` | team keys to pull from; empty = every team the key can see |
| `assignee` | none | only issues assigned to this user id, or `"me"` |
| `queued_states` | `["Todo"]` | workflow state names that mean "ready for the agent" |
| `required_labels` | `[]` | issues must carry one of these labels |
| `excluded_labels` | `["no-agent"]` | issues with any of these are ignored |
| `cycle` | `"any"` | which cycles to pull from, filtered server-side: `any`, `active` (alias `current`), `next`, `active-or-next`, or `none` (issues outside any cycle). The issue's cycle is also exposed to `priority.toml` as `cycle` / `cycle_number` |
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

## `[routing]`

Routing is machine configuration in `config.toml`. Defaults are `mode = "deterministic"`
and `rules = []`. `mode = "automatic"` requires `analysis.enabled = true`: explicit
choices win, then deterministic rules; unmatched tasks use Jev/Claude descriptions.
`mode = "explicit"` holds new tasks until `--project`, `task project` or dashboard
**P** selects a repository. Existing execution bindings are retained. See
[automatic repository selection](issue-analysis.md).
It selects only registered execution-project IDs; source admission still decides
whether an issue is approved, `priority.toml` decides ordering/models, and the
selected project's settings decide checkout, setup and cleanup.

```toml
# Register [projects.api.repo] and [projects.web.repo] first.
[linear]
execution_project = "" # require an explicit rule match (default: "default")

[[routing.rules]]
name = "API team backend work"
project = "api"
all = [
  { field = "source", op = "eq", value = "linear" },
  { field = "team", op = "eq", value = "API" },
  { field = "project", op = "eq", value = "Backend" },
]

[[routing.rules]]
name = "Shared GitHub feed web work"
project = "web"
all = [
  { field = "repository", op = "eq", value = "acme/issues" },
  { field = "label", op = "eq", value = "repo/web" },
]
```

Each rule has `name` (nonempty, unique), `project` (registered destination), and
`all` (at least one condition). These keys default to empty and validation rejects
incomplete rules. Conditions use the same strict `{field, op, value}` schema and
matching as [priority policy](priority.md): `eq`, `ne`, or `matches`
(regex) on `source`, `repository` (GitHub owner/repo), `team` (Linear team key),
`project` (Linear project name), or `label`. Equality and regexes ignore case;
qualified labels and child labels follow policy matching. A missing field does
not match `eq`/`matches`, but does match `ne`. Use `source` to scope negative rules.
Deterministic rules never evaluate title/body text. Automatic analysis uses that text
but can select only described registry IDs, never issue-provided checkout paths.

Explicit human selections override rules. Otherwise, in deterministic mode,
selection is independent of rule order:

1. Evaluate every rule; every condition in a rule must match.
2. If all matching rules name the same project, select it. These rules override
   `github.execution_project`, each `github_sources` fallback, or `linear.execution_project`.
3. If matching rules name different projects, hold the task and list the rules
   and destinations. There is no first-match tie breaker or fallback on conflict.
4. If no rule matches, use the source fallback. An explicit empty fallback holds
   unmatched work. Unknown destinations and malformed rules are configuration
   errors; previews and doctor explain them. No arbitrary repository is selected.

The initial decision and subsequent evaluations are stored with the task and logged
as `routing.selected` or `routing.blocked`. A deterministic assignment never changes during
sync, retry, restart or PR review, even while queued. Changed metadata/configuration
that selects another destination holds future starts; active sessions continue in
their original checkout. Restore the original route or deliberately run
`task project <task> <id>` for work without execution history. Unmatched intake has
an empty saved assignment: fix the routing and explicitly assign it before launch.
Started work requires a new task to move to another project. Manual tasks keep the
project explicitly chosen by `add --project` and do not use these rules.

`github sync` and `linear sync` are dry-run previews unless `--apply` is supplied;
`--json` includes each proposed routing decision. `task show` and `task explain`
show saved and current decisions; current routing uses locally stored metadata.
`doctor` diagnoses missing destinations, unmatched work and assignment conflicts.
The daemon re-evaluates routing before each launch pass and after setup; GitHub
also verifies current remote labels and approval before launching. Linear routing
uses the most recent successful source sync. A routing hold created during setup
logs `routing.launch_held` and preserves the worktree. Reload machine configuration
with `reload` (or restart) after changing rules.

## `[analysis]`

Optional model analysis; automatic routing can apply accepted repository choices.
Source approval and priority remain independent.
Machine-level settings only; `.powerqueue.toml` cannot enable analysis. See the
[workflow and tradeoffs](issue-analysis.md).

```toml
[analysis]
enabled = false                 # opt in before task analyze can contact models
jev_first = true                # uses priority.jev.endpoint/model and the Jev key
min_confidence = 0.8             # 0.5–1; Jev also requires probability and margin
timeout_secs = 20              # per request, 1–60; no retries

[analysis.projects]             # 1–32 registered IDs when enabled
# default = "API service: authentication, billing and database; excludes browser UI"
# web = "Browser UI: components and styling; excludes backend authentication"

# Optional server Claude Code fallback (existing login); absent by default.
# [analysis.claude_code]
# binary = "claude"              # no per-task placeholders
# model = "sonnet"

# Alternative API fallback; configure only one fallback.
# [analysis.llm]
# endpoint = "https://api.anthropic.com/v1/messages"
# model = "YOUR_MESSAGES_API_MODEL_ID"
# max_tokens = 1024             # 128–4096 output tokens
```

Project descriptions must be nonempty and at most 2,000 bytes each; only described
registered projects can be recommended. `jev_first` is independent of priority
scoring's `enabled` flag. With `jev_first = false`, configure a text model.
Endpoints require HTTPS, except loopback HTTP for local services/tests; embedded
credentials, query strings and fragments are rejected. Redirects are disabled.
The API fallback uses `secrets set analysis` or `POWERQUEUE_ANALYSIS_API_KEY`; Jev uses
`secrets set jev` or `JEV_API_KEY`. The API fallback has separate API billing. Claude Code uses its server login. Input/response limits and recorded token usage are described in
[issue analysis](issue-analysis.md#scheduling-staleness-and-cost).

## `[priority]` and `[priority.jev]`

| Key | Default | Meaning |
|-----|---------|---------|
| `file` | `<config>/priority.toml` | version 1 typed TOML policy; see [schema and Markdown migration](priority.md) |
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
| `max_concurrent` | `2` | parallel sessions; lowering this on reload keeps existing sessions alive and admits new work only after the live count falls below the new limit |
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
| `session_mode` | `"interactive"` | `interactive`: Claude Code runs in a tmux window you can attach to. `headless`: `claude -p --output-format stream-json --verbose` runs as a background process with no window; see [Headless sessions](#headless-sessions). The default for tasks no `session_mode_rules` row matches |
| `session_mode_rules` | `[]` | `[[claude.session_mode_rules]]` rows, each a `mode` and `all = [...]` predicates; the first matching row picks a task's mode; see [Choosing the mode per task](#choosing-the-mode-per-task) |
| `session_mode_overrides` | `{}` | `[claude.session_mode_overrides]`: task key → mode (keys are case-insensitive); beats every rule |
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

### Headless sessions

```toml
[claude]
session_mode = "headless"
permission_mode = "auto"          # or acceptEdits plus allowed_tools
```

With `session_mode = "headless"` powerqueue starts Claude Code tasks as
`claude -p --output-format stream-json --verbose` instead of the interactive
CLI. Everything else about the launch is the same: the prompt, `claude.env`,
the hook `--settings`, `--session-id`/`--resume`, `--permission-mode`,
`--allowedTools`, `--append-system-prompt`, `--effort`, `--fallback-model`
and `extra_args`. The process runs in the task's worktree in its own process
group, with no tmux window; it outlives a daemon restart the way a tmux window
does. Codex and Antigravity sessions always use tmux.

Each attempt writes three files under `<state>/tasks/<task id>/headless/`:

| File | Content |
|------|---------|
| `<session id>.<attempt>.stream.jsonl` | stdout: the `stream-json` events (`system/init`, messages, `system/api_retry`, the final `result`) |
| `<session id>.<attempt>.stderr.log` | stderr |
| `<session id>.<attempt>.exit` | the exit status, written when the CLI exits |

`powerqueue task show` names the log (`headless log`), and `powerqueue attach`
prints its path instead of attaching. The daemon reads the stream as it grows:

- **Progress**: every line is activity, so `scheduler.stale_session_secs`,
  `max_session_secs` and the dashboard work without a pane.
- **Usage**: assistant messages go to the budget ledger, deduplicated by
  message id with the Claude transcript, which is still tailed. The numbers
  match what an interactive session would record. The `result` message's
  estimated cost, turn count and duration are logged as a `session.result`
  event.
- **Rate limits**: `system/api_retry` events with `error = "rate_limit"` put
  Claude on its rate-limit cooldown (`budget.rate_limited`) while the run keeps
  retrying, and a run that fails on a usage limit throttles the task instead
  of crashing it.
- **Permission denials**: nobody can answer a permission prompt, so Claude
  Code denies the call and goes on; each denial is logged as
  `session.permission_denied` and counted by `powerqueue doctor`. Pick
  `permission_mode = "auto"` or list what tasks need in `allowed_tools`.

Hooks, `powerqueue task complete|block` and the DONE / BLOCKED markers work as
in an interactive session. When the process exits, the daemon decides how the
attempt ended (see [task lifecycle](task-lifecycle.md#headless-sessions)):
completed or blocked through those signals, throttled on a usage limit,
`crashed` (backoff, then `--resume` of the same session) on a non-zero exit,
a missing or failed `result`, or a turn that ended without a completion
marker. Cancel, retry, pause and the stale/timeout limits stop the process
with SIGTERM to its process group (SIGKILL 15 s later if it is still there).

The tradeoff: a headless session cannot be watched or steered mid-run.
`AskUserQuestion` and permission prompts have nobody to answer them, and
`powerqueue task send` refuses. A question ends the run (`needs_attention`);
`powerqueue task resume` or a reply on the issue resumes the same session.
Use headless mode for work that can run unattended. A repository can opt in
or out with `[claude] session_mode` in `.powerqueue.toml`. `claude.binary`
must not ask for a terminal (`docker exec -t`/`-it`): a headless session has
none, and `doctor` warns about it.

### Choosing the mode per task

Small, well-defined tasks can run headless while ambiguous or exploratory
ones stay interactive so you can attach. `[[claude.session_mode_rules]]` picks
the mode from the task's fields with the same predicates as `priority.toml`
(`all = [{ field, op, value }]`; fields `label`, `priority`, `estimate`,
`project`, `cycle`, `cycle_number`, `team`, `title`, `description`, `source`,
`key`, `repository`; ops `eq`, `ne`, `matches`, `gt`, `lt`; see
[priority rules](priority.md)). Every predicate of a row must hold; the
**first matching row wins**, so put the interactive exceptions first:

```toml
[claude]
session_mode = "interactive"          # tasks no rule matches
permission_mode = "auto"              # headless runs cannot answer prompts

# Exploratory work stays interactive, even when it is small or a chore.
[[claude.session_mode_rules]]
mode = "interactive"
all = [{ field = "label", op = "matches", value = "^(spike|design|needs-decision)$" }]

# Chores, docs and dependency bumps run headless.
[[claude.session_mode_rules]]
mode = "headless"
all = [{ field = "label", op = "matches", value = "^(chore|docs|deps)$" }]

# So does anything estimated at 2 points or less (estimate < 3).
[[claude.session_mode_rules]]
mode = "headless"
all = [{ field = "estimate", op = "lt", value = 3 }]

# One task, whatever the rules say.
[claude.session_mode_overrides]
"ENG-123" = "interactive"
```

Precedence, highest first:

1. `powerqueue task mode <task> interactive|headless` (`auto` clears it);
2. the task's key in `[claude.session_mode_overrides]`;
3. the first matching `[[claude.session_mode_rules]]` row;
4. `claude.session_mode`.

The mode is chosen when the task **first starts** and recorded on the task
with the reason (event `task.starting`), then kept for every later attempt:
a crashed headless task resumes headless, a review round runs in the same
mode, and a configuration reload never flips a started task. Only `task mode`
changes it, from the task's next start: a running session keeps its mode
until it ends. The mode applies to Claude Code sessions only; Codex and
Antigravity always run in tmux.

A headless task that blocks or asks a question goes to `needs_attention`
with its session stopped. To take it over in a window, switch it and resume:

```sh
powerqueue task mode ENG-123 interactive
powerqueue task resume ENG-123    # continues the same session in tmux; `task retry` starts over
powerqueue attach ENG-123
```

`powerqueue doctor` lists headless tasks waiting on a human with this advice.
`priority simulate` (a `mode` column; `--reasons` prints the full reason),
`task show`, `task explain` and the dashboard show each task's mode and what
chose it, or what its first start would choose. `config validate` lists the
rules and rejects unknown modes, invalid predicates and duplicate override
keys. A repository's `.powerqueue.toml` may replace the list with its own
`[claude] session_mode_rules`; `session_mode_overrides` stays global.

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
drafts of `priority.toml` and `config.toml`; see [Tuning with
`tune`](priority.md#commands-and-reload).

| Key | Default | Meaning |
|-----|---------|---------|
| `model` | `"sonnet"` | Claude model alias for the tuning session (`-m` overrides per run; must be a Claude model) |
| `timeout_secs` | `600` | kill the session after this long; the draft is kept and reported as failed |
| `extra_args` | `[]` | flags appended to the `claude -p` command line |
| `keep_drafts` | `20` | drafts to keep under `<state>/tune/`; older undone, unchanged, failed and invalid drafts are pruned after each run; running, proposed and applied drafts are retained for review, undo and intent comparison |

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
config for that repo, so the queue's policy can be versioned and reviewed with
the code. Precedence is repo over global, key by key: scalars and lists
replace the global value (lists are not merged), `instructions` and
`claude.append_system_prompt` are appended to the global value,
`prompt_template` and `priority_file` (relative to the repo root) replace
`prompt.template` and `priority.file`. Secrets, `[budget]`, `[repo]` (except
`setup`, `default_branch` and `branch_template`), `[tmux]`, `[logging]`,
`[tune]`, `[routing]`, `linear.endpoint`, `linear.execution_project`, `github.endpoint`, `github.execution_project`,
`github.token_env`, `github_sources` and `scheduler.gh_binary` stay
global: they describe the host, not the project. Unknown keys are rejected,
and the daemon then keeps its previous configuration (event
`daemon.reload_failed`; `config validate` and `doctor` show the problem).
A relative `priority_file` must stay inside the repository (`../x` is
rejected); `./` and doubled slashes are collapsed.

```toml
setup = ["npm ci"]
default_branch = "develop"
branch_template = "agent/{key}"
instructions = "Run `npm test` before you finish. Never touch migrations."
prompt_template = "docs/agent-prompt.md"   # see [prompt] above
priority_file = "docs/priority.toml"         # the rules file, versioned with the code

[linear]                     # everything in [linear] but endpoint and execution_project
queued_states = ["Todo"]
excluded_labels = ["agent/skip"]
assignee = "me"
post_comments = "questions"
done_state = ""              # "" clears a state the global config set

[github]                     # source settings except endpoint, execution_project and token_env
required_labels = ["agent:ready"]

[scheduler]                  # everything in [scheduler] but gh_binary
max_concurrent = 2
review_rounds_max = 5

[cleanup]
remove_worktree = false     # also: push_branch, delete_branch, keep_failed, run, close_tmux_window

[claude]
permission_mode = "plan"    # also: session_mode, session_mode_rules, effort, extra_args, allowed_tools, append_system_prompt

[codex]
approval = "on-request"     # also: reasoning_effort, extra_args

[gemini]
mode = "accept-edits"       # also: effort, extra_args
```

`config show` marks every key that came from the file (`# .powerqueue.toml`)
and names the file and the branch it was read from; `config validate` validates
the merged result, so a bad value set by the repo is reported; `doctor` has a
`.powerqueue.toml` check (readable, parses, which keys) and an `overrides
source` check (see below). `--json` output of `config show` and `config
validate` carries the same attribution under `repo_overrides`.

**Which copy of the file?** By default the daemon reads the file in the main
checkout's working tree (`repo.overrides_from = "working-tree"`), so whatever
is checked out there is what runs, and `doctor` warns when that checkout is
not on the default branch. Set `repo.overrides_from = "default-branch"` to
read `.powerqueue.toml` as committed on `origin/<default_branch>` instead
(the local default branch when the repository has no `origin`): a `git
checkout` in the main checkout changes nothing, and a change merged upstream
applies on its own after the next `git fetch`, which the daemon runs before
each task and every `repo.fetch_interval_secs` (300 by default). A relative
`priority_file` is then read from the same commit, so `priority.toml` can live
in the repository too; `priority show`, `priority check`, `priority explain`,
`priority simulate` and `doctor` read that same committed copy and say so
(`docs/priority.toml (origin/main)`), `priority edit` edits the working-tree
copy and reminds you to commit and push it. `tune --project ID` reads that
committed policy plus free-form intent, validates and simulates a proposal, then
prepares a working-copy change on approval. Review, commit and merge it through
the configured branch; tune never changes the source mode or writes a commit. What is read from the commit is only `.powerqueue.toml` and
`priority_file`: `prompt_template` is still read from the main checkout's
working tree, and `setup` / `cleanup.run` commands run inside the task's
worktree (which starts from the same `origin/<default_branch>`).

**Reloads.** The daemon fingerprints the file every 10 seconds (mtime and size
in the working tree; blob ids on the branch) and reloads its configuration
when it changed (event `daemon.reloaded`, with the keys taken), or only the
rules when nothing but the committed rules changed. As with `config set`,
new sessions use the new values; running sessions keep theirs. A working-tree
`priority.toml` (global or `priority_file`) is watched as before
(`priority.live_reload`). A change that cannot be applied (the file does not
parse, `config.toml` was mid-edit, git was busy) is reported once (event
`daemon.reload_failed`) and retried every check, so a transient failure
heals by itself; `doctor` ("config reload") says when the daemon is still on
the configuration from before such a change. A git failure while checking
is reported once (event `daemon.overrides_check_failed`) and never counts
as a change. The periodic fetch runs on its own thread: an unreachable
remote delays the next check, not the daemon.

**When the branch cannot be read** (`default-branch` mode and the branch is
missing, or git fails), the daemon does not start and a running daemon keeps
what it has; every other command runs with `config.toml` alone and says so
(`status`, `config show`, `doctor`), so the queue can still be inspected,
paused or stopped.
