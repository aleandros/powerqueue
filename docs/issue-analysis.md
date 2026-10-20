# Advisory issue analysis (prototype)

`powerqueue task analyze <task>` optionally classifies an existing issue, recommends
one described execution project and proposes preliminary next steps. It is an
explicit CLI operation, disabled by default. It does not run inside the daemon's
launch pass and does not invoke a coding session.

## Four separate decisions

1. **Analysis:** a model suggests a category, destination and plan. The saved
   result is advice, even when confidence is high.
2. **Approval:** a human decides whether the proposed work should happen. GitHub
   still requires its configured labels and trusted approvers. Analysis never
   adds `agent:ready`, posts source comments, or changes admission.
3. **Routing:** a human checks the destination against the registry, then uses
   `task project <task> <id>` for unstarted work and updates deterministic source
   routing if needed. Existing routing must agree. Started work cannot move.
4. **Prioritization:** priority rules and budget policy still determine when and
   with which model approved work runs. Analysis categories/confidence do not
   change scores, model preferences or resource reservations.

Keep work paused or unapproved while reviewing it. Analysis does not pause an
already runnable task; a concurrent daemon can still start previously approved
work. For issues not admitted into the queue, an initial prototype workflow is
`powerqueue add 'Issue title' --description - --paused`, then analyze that manual
task. It does not import arbitrary remote issues or bypass intake filters. Existing
Linear/GitHub tasks retain their original source association throughout analysis.

## Configuration and review

Configure registered project IDs and trusted descriptions in the machine
`config.toml`; see [configuration](configuration.md#analysis). Descriptions should
explain ownership and exclusions, not contain secrets. Only described projects
are candidates; registration alone does not make a project eligible for advice.

```sh
powerqueue secrets set jev
# Optional text fallback: a separate Anthropic Messages-compatible API key.
powerqueue secrets set analysis
powerqueue task analyze GH-123
powerqueue task analyze GH-123 --show
powerqueue --json task show GH-123
```

There are no default text-model credentials or model selection. Supply
`analysis.llm.model` and its API key explicitly. The text fallback uses the
[Anthropic Messages request format](https://platform.claude.com/docs/en/api/messages/create).
The key is stored through the existing keychain/0600-file mechanism, with
`POWERQUEUE_ANALYSIS_API_KEY` taking precedence. This is separate from Claude Code
subscription authentication and implementation-provider settings.

The command returns 0 for a current destination recommendation, 3 for unresolved
or stale advice, and the ordinary error exit for configuration/storage failures.
Exit 0 **does not approve execution**. `--show` performs no model requests and can
inspect results after analysis is disabled. `task show` includes the saved result
and its current status; `analysis.recorded` is an atomic audit event. `doctor`
reports invalid configuration and unresolved/stale advice on open tasks; `--fix`
does not approve, reroute, or call models.

## Jev first, text fallback

Jev's [Choice primitive](https://docs.typesafe.ai/primitives/choice) selects named
options, unlike the ordered Score primitive used for priority. One request asks
for destination and classification. Destinations have an explicit unresolved
option. We require a complete probability distribution, confidence and selected
probability at least `min_confidence`, plus a 0.15 lead over the next option.
Unknown or malformed destinations are rejected.

A confident Jev answer skips the text model. Jev does not generate a prose
explanation or implementation plan: the saved reasoning describes its statistical
evidence and the plan is explicitly a generic template requiring code inspection.
It is not presented as a code-specific investigation.

Missing keys, transport/API errors, malformed answers and ambiguous choices fall
back once to `analysis.llm`, if configured. That model receives issue text and
project descriptions and returns strict JSON with category, confidence, reasoning,
plan and a registered project or null. Unsupported destinations, extra fields,
low confidence, truncated output and tool-call responses remain unresolved.
If no fallback is configured/available, a human resolves the task. There is no
retry loop or daemon-side automatic rerun. Failed fallback attempts do not turn
uncertain Jev advice into a destination.

## Authority, resources and cost

The analysis service sees the issue title, description, labels, source identity
and source-project metadata, plus trusted descriptions. It gets no checkout
contents/paths, implementation prompt, process environment, shell, MCP tools or
source credentials. Issue text is JSON-encoded as data separate from instructions.
No output is executed or applied to source labels/configuration. Prompt injection
can still degrade advice; a human must verify the proposed work and destination.

Each invocation makes at most two HTTP requests, sequentially: one Jev request
and one fallback. Each has a 20-second default deadline (1–60 configurable), no
redirects or retries, a 64 KiB total input cap and a 128 KiB response cap. Oversized
input is rejected rather than silently truncating ownership clues. The fallback
has a 1,024-token output limit by default (128–4,096 configurable). These bounds
limit per-invocation spend, not an account-wide monetary budget or concurrent CLI
process count. No repository CPU/worktree/tmux slots are reserved.

Analysis API billing is independent of subscription pacing. Available input/output
token usage is saved for every response, including rejected proposals; failed or
timed-out calls may incur provider charges with unknown usage. No dollar estimate
is inferred from changing provider prices. Operators control frequency and API
spend limits at their provider. Do not invoke analysis repeatedly as a polling loop.

Saved results include an input/configuration fingerprint. Source edits, changed
project descriptions, model settings and registry changes make old advice stale;
rerun explicitly. Concurrent task updates are never overwritten. A later-started
analysis result cannot be replaced by an older request finishing late. Advice
records survive restarts, independently of task/session state.

## Validation and limits

Synthetic fixtures cover an API defect, API/UI ambiguity, insufficient detail,
misleading instructions, unsupported destinations, malformed probabilities and
JSON, unknown fields, tool attempts, missing credentials, HTTP failures, redirects,
response limits and timeouts. CLI tests exercise real SQLite, atomic event/result
persistence, stale input, concurrent task changes and offline diagnostics. Rust
properties validate destination/confidence gates; the stateful daemon model
interleaves advisory results with real admission, launch and lifecycle transitions.

These are deterministic contract and authority tests using mock model responses.
They do not measure live Jev/LLM routing accuracy or claim prompt-injection
resistance of the model itself. Before expanding automation, evaluate a labeled
corpus of your projects, measure ambiguity/false-routing rates and token costs,
and review generated plans. Automatic intake analysis, source publication,
automated approval, code inspection and automatic assignment are outside this
initial prototype. The existing repository triage workflows remain a separate
feature with their own execution permissions.
