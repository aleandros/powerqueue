# Repository-backed triage queues

Triage runs in a normal isolated worktree. Its deliverable is an analysis and a
recommended next action. Completing that Powerqueue task leaves the source
ticket open. Implementation is a separate, explicitly admitted coding task.

## Configure dedicated slots

Add this to the host `config.toml` (not `.powerqueue.toml`):

```toml
[scheduler]
max_concurrent = 2

[queues.default]
max_concurrent = 1

[queues.triage]
max_concurrent = 1
workflow = "triage"
all = [{ field = "team", op = "eq", value = "IT" }]

[workflows.triage]
instructions = ".powerqueue/workflows/triage.md"
completion = "leave-ticket-open"
comment_prefix = "[interno]"
```

`default` receives unmatched tasks and work already started before queues were
introduced. Other queues require `all` predicates using the priority grammar.
Multiple matching queues hold the task with an error; rule order has no precedence.
Repository routing remains independent: both queues can use the same execution
project. No duplicate checkout registration is required.

Queue limits are dedicated, never borrowed. Their sum must fit within the global
limit. Coding can occupy at most one slot in this example even when triage is
empty. Already running sessions are not killed when limits shrink; let them
finish before expecting the new capacity split. Provider budgets and rate limits
remain shared. This reserves concurrency, not a model allowance or CPU core.

With no `queues` configured the existing shared scheduling behavior is unchanged.
Queue and workflow policy are saved when a task is first scheduled, then retained
across restarts and retries. Removing a bound queue holds its future launches;
restore it rather than editing the database. `task show`, `status`, and the
dashboard show queue membership; `doctor` explains missing queues and files.

## Version the workflow with the code

Commit [the example workflow](examples/triage-workflow.md) as
`.powerqueue/workflows/triage.md` in the application repository. The host config
names the path; the worktree's revision supplies its contents. Paths must remain
inside the checkout, including symlinks. Missing, empty, or invalid workflow
files fail the launch instead of falling back to coding instructions.

A selected workflow replaces `prompt.template` and `prompt.instructions`, and
receives the normal task context and completion protocol. Workflow Markdown
supports the same `{{placeholders}}` as prompt templates. Shared repository
instructions and provider system prompts still apply: make their coding/PR
requirements explicitly conditional on an implementation task.

`completion = "standard"` keeps normal lifecycle and cleanup behavior.
`leave-ticket-open` suppresses Powerqueue's source state changes at start, failure,
and completion, prevents PR adoption, and suppresses automatic commit/push during
cleanup. Clean worktrees are removed normally; uncommitted files or unpushed
commits are preserved for inspection. The policy controls Powerqueue's actions;
it does not sandbox arbitrary tools available to the agent.

Analysis tasks may inspect parent tickets and tickets with unresolved blockers.
Their policy also prevents Powerqueue's parent watcher from closing them after
coding children complete. For parents never admitted as tasks, use a queue rule
based on their team so the watcher can resolve the same policy.

## Admit IT without broadening coding intake

Keep the existing AVS intake unchanged and add an independent filter:

```toml
# In config.toml or the legacy repository's .powerqueue.toml.
# Existing [linear] team_keys, assignee, cycle and excluded_labels stay as-is.
[linear.team_filters.IT]
queued_states = ["Todo"]
required_labels = ["triage/requested"]
excluded_labels = ["no-agent"]
cycle = "any"
max_issues = 100
```

Each entry owns intake for that team, including when the base filter covers all
teams. Unspecified fields use independent defaults: any assignee, `Todo`, any
cycle/project, no required labels, `no-agent` excluded, and a cap of 100.
Omitting `agent/skip` from IT's exclusions is intentional: it continues to prevent
coding admission while permitting this analysis workflow. Do not remove that
label from help-desk tickets. Start by explicitly labeling a few tickets
`triage/requested`; removing `required_labels` later admits all matching IT tickets.

## Record and publish an analysis

The agent writes a UTF-8 Markdown report in its task state directory.
Agent sessions receive `POWERQUEUE_TASK_DIR` pointing to that directory. Container
launchers must mount it, as they already do for launch files and the shim inbox;
an explicit provider environment value can override its path inside the container.
The agent then records the result:

```sh
powerqueue task complete IT-123 --summary-file /path/to/report.md --outcome needs-information
```

`--summary-file` and `--summary` are alternatives. `--outcome` is only accepted
for a `leave-ticket-open` workflow and conflicts with `--pr`. The container shim
supports the same report file and outcome arguments. A bare done marker cannot
complete an analysis that has not recorded a report and outcome.

Create these labels in Linear before enabling publication (either qualified flat
names or children of a `triage` group, visible to the IT team):

| Outcome / label | Meaning |
| --- | --- |
| `triage/needs-information` | Draft specific questions; a human decides what to send |
| `triage/needs-decision` | Expected behavior, scope, or ownership needs a decision |
| `triage/ready-for-agent` | Actionable recommendation; **not** coding authorization |
| `triage/manual-action` | Support or operational work is the next step |
| `triage/no-action-recommended` | Explain the evidence for no change or a duplicate |

Also create `triage/requested` if using the opt-in intake example. Linear label
groups are single-select, so when `requested` lives in the same `triage` group the
recommendation label replaces it on the issue once the report is published (the
completed task is not re-imported either way).

Powerqueue publishes a report containing the recommendation, inspected base SHA,
and full summary, then sets the recommendation label. It removes only labels the
issue actually carries: any other recommendation label and, when the recommendation
is a group child, its siblings in that group (such as `triage/requested`). Other
labels, including `agent/skip`, are preserved using additive/removal mutations.
`comment_prefix` is the first text in the comment; use the marker your help-desk
integration recognizes for internal notes. It also prefixes other workflow
comments. `linear.post_comments = "questions"` or `true` enables publication;
`false` disables it. Native report/label delivery currently targets Linear.

Delivery receipts and a client-generated comment UUID are saved before posting.
Network failures and missing labels are retried with exponential backoff (one
minute, doubling, capped at one hour) and survive restarts. Each tick attempts at
most one report: never-failed reports go first and retries take turns oldest first,
so failing receipts do not delay newly completed analyses.
The execution slot is already free while delivery waits. `doctor` reports
pending publication; `task show` records `triage.completed`, `triage.published`,
or `triage.delivery_failed` events. Powerqueue never creates labels implicitly.

## Decide what happens next

Read the internal report and choose the next action. Customer questions remain
drafts. Use the existing help-desk action to send them. For implementation,
create/admit a linked engineering ticket with the findings and acceptance criteria;
the recommendation label does not automatically do this.

A completed triage task is not automatically re-imported while its issue remains
open. New comments alone do not restart it. After new information, run
`powerqueue task retry IT-123` to request another analysis pass. This preserves
the workflow, refreshes Linear intake, and records a new report. Normal worktree
retry semantics apply: retained worktrees/branches are reused, so the report's
recorded revision matters. Automatic retriage and automatic coding handoff are
not part of this first version.

## Aceros VS application rollout

The inspected application already has an IT help desk, `agent/skip` on its
tickets, a way to create linked AVS engineering tickets, and internal notes
recognized by a leading `[interno]`. No portal schema change is required.

1. Commit `.powerqueue/workflows/triage.md` using the example, adapted to the
   application's bounded contexts, help-desk terminology, and Spanish reports.
2. Add a short section to `AGENTS.md`: analysis workflows inspect and report;
   `/fix-ticket`, `create-pr`, `/ship-pr`, and deployment completion rules apply
   only to implementation tasks. Leave the existing coding prompt unchanged.
3. Add `[linear.team_filters.IT]` above to `.powerqueue.toml`. Preserve AVS's
   `team_keys = ["AVS"]`, `assignee = "me"`, `cycle = "active"`, and
   `excluded_labels = ["agent/skip"]`. Preserve the existing bootstrap/teardown.
4. Extend the help-desk Linear setup script to ensure the `triage` group and
   labels exist, or create them manually in Linear. Label creation is a separate
   rollout action, not something the Powerqueue PR performs on production.
5. Add the host queue/workflow tables to the reference
   `docs/agent/powerqueue/config.toml`, and document the triage/handoff process in
   `docs/agent/powerqueue/README.md` or the agent runbook.

On the inspected `dev`, the service uses `/home/edgar/.local/bin/powerqueue`,
the application checkout is `/home/edgar/ofertacero`, and the installed binary
reports 0.18.1. The shell's default PATH did not include that executable.
The application currently limits total concurrency to two for CPU reasons;
start with one coding slot and one triage slot. Keeping two coding slots plus
one triage slot would require raising the effective global limit to three,
including the repository's scheduler override, after checking capacity.

Deploy a release containing this feature only after the application workflow is
on the fetched default branch. Add host settings, run `config validate`,
`linear sync` (dry run), and `doctor`, then restart the service and opt in one
IT ticket. Review its internal report and label before expanding intake.
The implementation PR does not edit the application or deploy/restart `dev`.
