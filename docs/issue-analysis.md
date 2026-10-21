# Repository selection with Jev and Claude

Describe each registered execution project once. With `routing.mode = "automatic"`,
powerqueue chooses a repository for new, unmatched tasks using Jev. A confident
choice is applied automatically; uncertain results fall back once to Claude Code
or a configured text API. If that result is also uncertain, the task stays held
until you choose. Source approval, pause/deferral settings and budget policy still
control whether and when implementation starts.

## Application and infrastructure example

```toml
[projects.app.repo]
path = "/srv/application"
[projects.infra.repo]
path = "/srv/infrastructure"

[routing]
mode = "automatic"              # default: deterministic; alternative: explicit

[analysis]
enabled = true
min_confidence = 0.8
jev_first = true
timeout_secs = 20

[analysis.projects]
app = "Product application: screens, API behavior, authentication and business logic. Excludes provisioning and deployment infrastructure."
infra = "Infrastructure: Terraform, networks, servers, Kubernetes and deployment pipelines. Excludes application features and business logic."

[analysis.claude_code]
binary = "claude"
model = "sonnet"
```

Set the Jev key with `powerqueue secrets set jev`, sign in to Claude Code on the
server, and reload/restart powerqueue. Keep the normal Linear/GitHub intake and
approval configuration. Descriptions reference registered IDs, never paths supplied
by an issue. Only described projects are eligible for model selection.

“Fix the login form” can go to `app`, while “Change the Terraform network” can go
to `infra`. “Improve login reliability” may need the Claude fallback or your choice.
The threshold is a model-confidence gate, not a measured guarantee of 80% accuracy.

## Choose or correct a repository

```sh
powerqueue add 'Fix the login form'               # automatic selection
powerqueue add 'Update the network' --project infra # explicit selection
powerqueue task project TASK-123 app              # override before execution
powerqueue task analyze TASK-123                  # retry now
powerqueue task analyze TASK-123 --show            # inspect saved evidence
```

In the dashboard, select an unstarted task and press **P** (uppercase). Choose a
repository with arrows or `j`/`k`, then Enter; Escape cancels. Lowercase `p` still
pauses. The picker stays attached to the selected task during refreshes and checks
eligibility again when applied. Active or previously executed tasks cannot move;
create a new task for another repository.

An explicit selection is sticky and wins over model results and routing rules,
including a model request already in progress. It does not grant source approval.
To require human selection for every new task, set `routing.mode = "explicit"`.
CLI `add` then requires `--project`; dashboard creation opens the picker. Cancelling
leaves the task held. Source intake also stays held until explicitly assigned.

The default `deterministic` mode preserves source fallback/rule routing and keeps
`task analyze` advisory. In `automatic` mode explicit selections win first, then
matching deterministic rules. Conflicting rules hold work rather than call a model.
Only unmatched work uses descriptions instead of the source fallback. Existing
execution bindings stay fixed across retries and reviews.

## Evidence and fallback

Jev's [Choice primitive](https://docs.typesafe.ai/primitives/choice) selects a
project and category, with an explicit unresolved option. A choice needs confidence
and selected probability at least `min_confidence`, plus a 0.15 lead over the next
option. It provides statistical evidence and a generic inspection plan, not a
code-specific explanation.

An uncertain choice, unavailable key or malformed response falls back once. Claude
receives task text, labels, source metadata and trusted project descriptions. It
returns strict JSON with destination, category, confidence, reasoning and a
preliminary plan. Unsupported destinations, unknown fields, low confidence,
malformed responses and timeouts remain unresolved. No code is inspected.

`analysis.claude_code` uses the server's Claude Code login and its configured model.
The request runs in a temporary directory with tools, MCP servers, hooks, slash
commands, user/project settings and session persistence disabled. The CLI flags
are documented in the [Claude Code reference](https://code.claude.com/docs/en/cli-reference).
A missing or incompatible installation leaves diagnostic evidence and holds work.
The binary accepts a command template without per-task placeholders.

Alternatively, omit `analysis.claude_code` and configure `analysis.llm` with an
Anthropic Messages-compatible endpoint/model and `powerqueue secrets set analysis`
(or `POWERQUEUE_ANALYSIS_API_KEY`). Configure only one fallback. API authentication
and billing are separate from Claude Code's login. Neither fallback is enabled
unless configured.

## Scheduling, staleness and cost

The daemon runs at most one routing request chain at a time, outside its scheduling
tick. It reserves no worktree, tmux window or implementation slot. A chain has at
most one Jev call and one fallback, each with a configurable 1–60 second deadline
(default 20), 64 KiB input and 128 KiB response limits. HTTP requests have no redirects
or retries. Claude Code is limited to one turn; the API fallback defaults to 1,024
output tokens. Available token usage and failures are saved in analysis records.
These bounds do not impose an account-wide spend limit or limit concurrent manual
CLI calls. Claude Code routing uses the server account but is not charged to the
scheduler's implementation ledger.

Current unresolved results are cached across restarts. `task analyze` explicitly
retries them, and can also replace a previous automatic decision before execution.
A new confident result may select another repository; an unresolved replacement
clears the automatic selection and holds the task. Explicit human choices remain
unchanged. Changed task text, descriptions, routing policy, models or registry
bindings invalidate an automatic selection and trigger new analysis before an
unstarted task can launch. A saved in-flight marker prevents automatic repeated
spend after a crash; retry interrupted analysis with `task analyze`. `run --offline`
makes no automatic model calls. `run --once` waits for its one pending analysis
and any setup, but newly routed work may need the next admission pass.

Results and assignment changes are saved together transactionally against the latest
task. Stale results, explicit choices, sessions and execution history prohibit
reassignment. Model output never
changes source approval, labels, state, priority, deferrals or budget. Source sync
continues to use the original issue association. `analysis.recorded`,
`routing.automatic_assigned`, `routing.automatic_withheld` and routing events explain
decisions; `task show`, `task explain` and `doctor` expose pending, unresolved and
stale work.

`task analyze` exits 0 for a current destination proposal, 3 for unresolved/stale
results. Exit 0 does not mean the task was approved or started; JSON includes
`repository_assigned` (false when a selection was withheld). CLI analysis applies
the same repository source overrides as the daemon. `--show` never assigns or calls
a model. `doctor --fix` does not choose repositories or invoke models.

## Validation

Mock Jev/API responses and a fake Claude Code executable test thresholds, ambiguity,
unsupported destinations, misleading issue instructions, response bounds and
fallbacks. SQLite tests cover atomic assignment, stale writers and manual overrides.
Properties and stateful models interleave analysis, source/config edits, approval,
assignment, launch and restart. Dashboard tests cover selection across refreshes.
These are contract tests, not measurements of live routing accuracy. Use your own
labeled tasks to tune descriptions and confidence thresholds.
