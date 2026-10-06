# PRIORITY.md: the rules grammar

`PRIORITY.md` decides three things for every task: its **criticality**
(`critical`, `high`, `normal`, `low`), its **score** (higher runs first), and
optionally its **model**. It is ordinary Markdown so it can be edited anywhere
and reviewed like code. `powerqueue init` writes the reference template
(`src/priority/PRIORITY.template.md`); `powerqueue priority path` prints where
the live file is.

Check a file with `powerqueue priority check`. See what it does to one task
with `powerqueue priority explain ENG-123`, or to the whole queue with
`powerqueue priority simulate` (see [Tuning](#tuning-with-priority-simulate)).

## Document structure

A document is a sequence of `##` sections. Section names are matched
case-insensitively. Anything outside a known section (prose, HTML comments,
other headings) is ignored, so you can keep notes in the file.

| Section | Purpose |
|---------|---------|
| `## Critical`, `## High`, `## Normal`, `## Low` | rules that assign a criticality |
| `## Default` | criticality for tasks no rule matches (default `normal`) |
| `## Scoring` | point adjustments |
| `## Overrides` | per-ticket pins |
| `## Models` | preferred models per criticality (`fable \| gpt-6.1-sol`), plus conditional `if <conditions>: <models>` rows |
| `## Jev` | Jev question and rubric |

Rules are bullet lines (`- ...` or `* ...`). Blank lines and non-bullet lines
inside a section are ignored, and HTML comments (`<!-- ... -->`, even across
lines) are stripped; there is no `#` line comment. Unknown sections, bullets
before the first `##`, a repeated `## Default` or `## Models` entry and unknown
Jev settings produce warnings, which `priority check` and `doctor` report with
line numbers. A bullet that cannot be parsed (unknown field, bad operator,
invalid regex, unknown model name) is an error.

## Conditions

A condition compares one task field with a value.

| Form | Meaning | Example |
|------|---------|---------|
| `field: value` | equals, case-insensitive; for `label`, membership in the label list (`=` and `==` are accepted too) | `label: incident` |
| `field != value` | not equals (for `label`: label absent) | `project != Sandbox` |
| `field ~ regex` | case-insensitive regex match (for `label`: any label matches) | `title ~ "^hotfix"` |
| `field > n` | numeric greater than (`priority`, `estimate`, `cycle_number` only) | `estimate > 8` |
| `field < n` | numeric less than (`priority`, `estimate`, `cycle_number` only) | `priority < 3` |

Join conditions with ` and `. All conditions in a rule must hold. There is no
`or`: write a second bullet instead.

```markdown
## Critical
- priority: urgent and label: customer
- label: incident
```

Quotes around values are optional; use them when a value contains spaces or
regex metacharacters.

### Grouped labels (`parent/name`)

Linear labels can live in a group (a parent label): the `fable` label inside
the `model` group is a different label from a loose `fable` label.
powerqueue stores a child label qualified with its group, `model/fable`, and
a loose label as plain `fable` (`task show` lists them that way). Labels on
existing tasks are re-synced on the next Linear poll.

- `label: model/fable` matches only the child label in the `model` group.
- `label: fable` (no `/`) matches a loose `fable` label **and** any child
  label called `fable`, so rules (and `linear.required_labels` /
  `excluded_labels`) written before labels were qualified keep working.
- `label ~ regex` matches the stored form or the bare child name:
  `label ~ ^model/` matches any label in the `model` group, and an older
  `label ~ ^fable$` still matches `model/fable`.
- `/` is the group separator, so a label whose own name contains `/` is
  ambiguous: a loose label named `frontend/web` also matches `label: web`,
  and a child `ios/android` in group `platform` is stored as
  `platform/ios/android` (write that, or `label: android`, to match it).

## Fields

| Field | Type | Values |
|-------|------|--------|
| `label` | list of strings | any Linear label, or labels given with `add --label`; a child label in a Linear label group is stored as `parent/name` (see below) |
| `priority` | enum / number | `urgent`, `high`, `normal` (`medium`), `low`, `none`, or Linear's 0–4 (0 = none, 1 = urgent) |
| `estimate` | number | Linear estimate points |
| `project` | string | Linear project name |
| `cycle` | enum | `active`, `next`, `past` or `future`: where the issue's cycle sits today; missing when the issue is in no cycle (and on manual tasks) |
| `cycle_number` | number | the cycle's number (`Cycle.number` in Linear); missing without a cycle |
| `team` | string | Linear team key (`ENG`); missing on manual tasks |
| `title` | string | task title |
| `description` | string | task description (Markdown) |
| `source` | enum | `linear` or `manual` |
| `key` | string | task key (`ENG-123`, `manual-1a2b3c4d`) |

Comparing a missing field (no estimate, no project) with `:` or `~` never
matches; `!=` matches.

## Criticality sections

```markdown
## Critical
- priority: urgent
- label: security

## High
- priority: high
- project ~ "Launch"

## Normal
- priority: normal

## Low
- label: chore
- label: docs

## Default
- normal
```

Evaluation walks the sections from `Critical` down to `Low` and stops at the
first section with a matching bullet. A task that matches nothing gets the
`## Default` criticality, which is `normal` when the section is absent. The
`Default` section holds a single bullet with one of the four names.

Criticality is what the budget policy looks at when it decides whether a tier
is eligible (see [budget.md](budget.md)). Its base score is:

| criticality | base score |
|-------------|-----------:|
| critical | 1000 |
| high | 500 |
| normal | 100 |
| low | 10 |

## Scoring

```markdown
## Scoring
- +40 if label: customer
- +20 if priority: high
- -30 if estimate > 8
- +10 if source: manual
```

Each bullet is `+N if <conditions>` or `-N if <conditions>` (the `if` is
optional). Every matching bullet adds its delta; bullets are independent.
Deltas are plain numbers (decimals allowed). Base scores are 10/100/500/1000, so a delta of a few
hundred can reorder tasks across criticality levels; keep deltas below 400 if
you want criticality to dominate.

## Overrides

Pins for individual tickets. The key is the task key, case-insensitive.

```markdown
## Overrides
<!-- KEY: critical|high|normal|low, KEY: +N / -N, KEY: model = <tier>, KEY: skip -->
- ENG-123: critical
- ENG-200: +100
- ENG-201: model = opus
- ENG-300: skip
```

One ticket may have several override bullets. `skip` moves the task to
`paused` with the last error `skipped by PRIORITY.md`; remove the line and
the daemon re-queues it on the next tick. A criticality override beats the
section rules. A score override is added after scoring rules. A model
override is a *preference list* handed to the budget policy (`KEY: model =
fable | gpt-6.1-sol`, most wanted first; the alternatives may belong to
different providers), which tries each entry through its provider's
downgrade chain and may still pick something else when nothing on the list
fits (it says so in `task explain`); only `task model` and `add --model` set
a hard `model_override`, and even that is downgraded within its provider
rather than ignored.

## Models

```markdown
## Models
- critical: fable | gpt-6.1-sol
- high: opus | gpt-6-astra
- normal: sonnet
- low: sonnet
```

```markdown
## Models
- if label: model/fable: fable
- if label ~ ^model/ and priority: urgent: opus | gpt-6.1-sol
- critical: fable
```

Maps a criticality to a list of preferred models, most wanted first.
A bullet starting with `if` is a **conditional row**: `if <conditions>:
<model> [| <model>...]`. Conditions use the same grammar as the criticality
sections (`label: model/fable`, `label ~ regex`, `and`, ...). The conditions
and the model list are split at the first `:` where both halves parse, so
`if title ~ a:b: fable` and `if label: x: codex:gpt-6` work. Conditional
rows are tried in file order and the first one whose conditions all hold
wins over the criticality row (an `## Overrides` model list still wins over
both); a row repeating an earlier row's conditions is a warning, since it can
never apply. They do not change the task's criticality or score.

A conditional row is still a preference: the budget reservation
(`budget.providers.<p>.models.<m>.min_criticality`, fable is reserved for
`critical` by default) applies, so `if label: model/fable: fable` on a
`high` task runs a downgrade until the reservation relaxes late in the
period. `priority check` and `priority explain` print a `note:` when that
happens. When a row changes a task's preferred models the daemon logs a
`task.models_changed` event and refreshes the task's reasons.

Alternatives are separated by `|` (whitespace around them does not matter;
`critical = fable | gpt-6.1-sol` also works). Each name is parsed with the
same rules as `task model`: Claude's aliases (`fable`, `opus`, `sonnet`,
`haiku`), `gpt-*`/`o1*`/`o3*`/`o4*`/`codex*` for Codex, `gemini-*` for
Antigravity, or an explicit `codex:<name>` / `gemini:<name>`; an unknown
name is an error with the line number and those rules, and a name listed
twice is a warning (the repeat is ignored). A model whose provider is
disabled in `config.toml` is simply skipped by the policy (`budget show`
lists disabled providers).

Missing levels leave the choice to the budget policy: `normal` falls back to
`budget.default_model`, `low` to `budget.low_model`, and `critical`/`high`
take the most capable eligible model. As with override models, this is a
preference: the policy tries each alternative in order (each through its own
provider's downgrade chain) and, when none is eligible, the first eligible
model in `budget.provider_order`. `priority check` prints the lists
(conditional rows first, with their line numbers); `priority explain` shows
which one applied, e.g. `model: fable (if label: model/fable)` or `model:
opus (high row)`, and `priority simulate` tags models chosen by a
conditional row the same way.

## Jev

```markdown
## Jev
- enabled: true
- question: How important is it to ship this ticket this week for a small product team?
- levels: can wait indefinitely | nice to have | important this week | blocking customers or revenue
```

Jev (TypeSafe "System One") answers a fixed-form question with a probability
over ordered rubric levels. powerqueue sends the ticket (title, description,
labels, criticality, Linear priority, project) with `question` as the
instructions and `levels` (pipe-separated, lowest first, 2 to 10 entries) as
the rubric. `instructions` and `criteria` are accepted as aliases. The reply's
probability-weighted level index is divided by `levels - 1`, clamped to 0..1
and multiplied by `priority.jev.weight` (default 300 points).

Jev needs a key (`powerqueue secrets set jev`) and `priority.jev.enabled = true`
in `config.toml` *and* `enabled: true` in this section; the section controls
the question. Scores are cached per ticket (`jev_scores` table) by content
hash of title, description and labels and re-requested when the ticket
changes (`priority.jev.rescore_on_change`). When a request fails the cached
score is kept and the error is logged once (`jev.error` event).

## Evaluation order and precedence

For one task, at every re-score:

1. **Skip**: if an `## Overrides` bullet says `skip`, the task is marked
   `skip` and nothing else matters.
2. **Criticality**: override bullet, else the first matching section
   (`Critical` → `High` → `Normal` → `Low`), else `## Default`.
3. **Score** = base score of the criticality
   `+` every matching `## Scoring` delta
   `+` any `## Overrides` score delta
   `+` Jev normalised score × `priority.jev.weight` (when enabled)
   `+` hours waiting × `priority.age_boost_per_hour`, capped at 200 points.
4. **Model**: the CLI (`task model`, `add --model`) wins, then the
   `## Overrides` model list, then the first matching `## Models` `if` row,
   then the `## Models` list for the criticality, else none. The list is handed to the budget policy as the preferences
   (`Evaluation.models`; `Evaluation.model` is its first entry): each entry
   is tried in order, downgraded within its own provider when it is out of
   budget, never upgraded above it; when nothing on the list fits the
   policy falls back to `budget.provider_order` (see [budget.md](budget.md)).

Every step records a reason, shown by `task explain`, `task show` and the
dashboard. The exact strings (`src/priority/rules.rs::evaluate`):

| Step | Reason |
|------|--------|
| skip override | `skip: override for ENG-300` |
| criticality override | `critical: override for ENG-123` |
| section rule | `critical: matched rule at line 5 (label: incident)` |
| no rule matched | `normal: default (no rule matched)` |
| base score | `base 1000 (critical)` |
| scoring rule | `+40 scoring line 21 (label: customer)` |
| score override | `+100 override for ENG-200` |
| Jev | `jev 0.48 × 300 = +144` |
| age | `age +4.0 (2.0h)` |
| model from overrides | `model opus from ## Overrides` |
| model from a Models `if` row | `model fable from ## Models line 30 (if label: model/fable)` |
| model from Models | `model fable \| gpt-6.1-sol from ## Models` |

Ties in score are broken by criticality, then by age (older first)
(`scheduler::pick_next`).

## Live reload

With `priority.live_reload = true` (default) the daemon watches the file's
directory with the OS file watcher and falls back to mtime polling when a
watcher cannot be created (network drives). On each tick the daemon checks the
flag, re-parses the file and re-scores every open task that has no live
session (running tasks keep their score until they finish). A file that
fails to parse is reported as a `rules.invalid` event, in the log and by
`doctor`, and the previously loaded rules stay in effect, so a typo never
empties the queue (`scheduler/daemon.rs::load_rules`). A missing file logs
`rules.missing` once and falls back to the built-in defaults (every task
`normal`, no scoring, no models). Editing with `powerqueue priority edit` or
any editor works the same way. `powerqueue stop` is never needed to apply
rules.

## Tuning with `priority simulate`

`powerqueue priority simulate` is a dry run of the whole pipeline for every
open task: it re-scores each task with the rules, ranks them exactly as
`scheduler::pick_next` does (score, then criticality, then age), runs the
first `scheduler.max_concurrent` schedulable tasks through the budget policy
with the current ledgers, and prints the result. Nothing is written: the
stored scores, the tasks and the ledgers are untouched.

```text
$ powerqueue priority simulate
Simulated queue with ~/.config/powerqueue/PRIORITY.md (max 2 concurrent; nothing was written)
 #    was  task    state   criticality        score       prefers  policy would run  title
 1 ▶  =    INC-1   queued  normal → critical  100 → 1000  fable    fable             Fix outage
 2 ▶  ↑3   CUS-3   queued  normal             100 → 140   opus     opus              Customer ask
 3    ↓2   CH-2    queued  normal → low       100 → 10    sonnet   sonnet            Tidy docs
 4    =    ENG-77  skip    normal             100         opus     –                 Already taken
```

- `was` is the task's rank with the scores stored by the daemon; `↑3` means
  it moved up from third place, `=` that it stays, `new` that it is not in
  the queue yet (`--linear`).
- `criticality` and `score` show `stored → simulated` when the rules change
  them.
- `prefers` is the model list the rules hand to the policy; `policy would
  run` is what the budget policy picks for that task right now (or why it is
  throttled). `▶` marks the rows that would start on the next tick.

Options:

| Flag | Effect |
|------|--------|
| `--file PATH` | try a draft file instead of the live one; the daemon keeps using the live file until you copy the draft over it |
| `--config PATH` | try a draft `config.toml` too (budget shares, concurrency); the repo's `.powerqueue.toml` still applies |
| `--linear` | also fetch the queued issues from Linear and rank the ones not in the queue yet (nothing is stored) |
| `-a`, `--all` | include completed/failed/cancelled tasks, to check rules against history |
| `--reasons` | print every rule that fired for each task, plus the policy's reason |
| `--no-budget` | skip the budget policy (rank only) |
| `-n N` | show only the first N rows |
| `--json` | the rows as JSON (`rank`, `stored_rank`, `criticality`, `score`, `preferred_models`, `model`, `policy`, `would_start_now`, `reasons`, ...) |

Typical loop: copy `PRIORITY.md` to a draft, edit, `priority simulate --file
draft.md`, repeat until the order looks right, then move the draft over the
live file. With `priority.live_reload = true` (the default) the daemon
re-scores every task without a live session on its next tick.

## Tuning with `powerqueue tune`

`powerqueue tune "<what you expect>"` runs that loop for you with a headless
Claude Code session. It copies `PRIORITY.md` and `config.toml` into
`<state>/tune/<id>/`, writes a `CONTEXT.md` with the current simulation
(`--reasons`), every open task's matchable fields and the budget state, and
sends Claude the request together with this document and the config
reference. Claude edits the drafts and verifies them with `priority check
--file`, `priority simulate --file --config` and `config validate --file`
(the only commands it may run). powerqueue then parses both drafts, refuses
anything that does not validate, shows Claude's summary, the diff and the
simulated queue with the drafts, and asks before replacing the live files
(`-y` skips the question, `--dry-run` never applies, `--apply` applies a kept
proposal later, `--undo` restores the previous files). Requests the rules
cannot decide, such as an order set by the budget policy, come back as an
explanation and no change.

Good requests name what you saw and what you expected: "ENG-12 should rank
above ENG-40 because customer bugs come first", "anything labelled `chore` is
low and runs on sonnet", "skip ENG-77, someone else took it", "tasks from the
active cycle before everything else". `--scope priority` keeps Claude out of
`config.toml`; `--scope config` is for "run three tasks at once" or "give
fable 40% of the week".

## Idioms

**Pin a ticket to the front of the queue**

```markdown
## Overrides
- ENG-451: critical
- ENG-451: +500
```

**Keep Fable for incidents**

```markdown
## Critical
- label: incident
- label: security

## Models
- critical: fable
- high: opus
- normal: sonnet
```

Pair with `budget.providers.claude.models.fable.min_criticality = "critical"` (the default) so
only critical tasks touch Fable early in the period.

**Spill critical work onto a second subscription**

```markdown
## Models
- critical: fable | gpt-6.1-sol
- high: opus | gpt-6-astra
- low: gpt-6-luna | haiku
```

With `budget.providers.codex.enabled = true`, an incident still goes to
Fable while Claude has room and to `gpt-6.1-sol` when it does not (a
rate-limited or exhausted provider is skipped until its reset). Listing
`gpt-6-luna` first for `low` sends chores to the Codex budget and keeps
Claude's for everything else.

**Deprioritise chores without blocking them**

```markdown
## Low
- label: chore
- label: docs
- title ~ "^(chore|docs)[:(]"

## Scoring
- -50 if label: chore
```

`priority.age_boost_per_hour` (2 points per hour, at most 200) still lets a
chore surface after a quiet day.

**Prefer the current cycle**

```markdown
## Scoring
- +150 if cycle: active
- -100 if cycle: future
```

Issues in the active cycle jump ahead of everything else at the same
criticality; issues planned for a later cycle wait. Issues in no cycle match
neither line and keep their base score. `cycle_number > 14` works too when
you want to pin a specific sprint. To not pull later cycles at all, set
`linear.cycle = "active"` (or `"active-or-next"`) in `config.toml` instead:
that filters on the server, so those issues never become tasks.

**Let Jev break ties**

```markdown
## Jev
- enabled: true
- question: How much would shipping this ticket this week matter to paying customers?
- levels: not at all | a little | noticeably | it is blocking them
```

With `priority.jev.weight = 300`, Jev can lift a `normal` ticket (base 100)
above other `normal` tickets but not above a `high` one (base 500). Raise the
weight if you want Jev to cross levels.

**Skip tickets a human is already on**

```markdown
## Overrides
- ENG-77: skip
```

The task shows as `paused` until the line goes away. Or add the `no-agent`
label in Linear (`linear.excluded_labels`), which keeps the issue out of the
queue entirely.

**Prefer small tickets**

```markdown
## Scoring
- +30 if estimate < 3
- -30 if estimate > 8
```
