# PRIORITY.md: the rules grammar

`PRIORITY.md` decides three things for every task: its **criticality**
(`critical`, `high`, `normal`, `low`), its **score** (higher runs first), and
optionally its **model**. It is ordinary Markdown so it can be edited anywhere
and reviewed like code. `powerqueue init` writes the reference template
(`src/priority/PRIORITY.template.md`); `powerqueue priority path` prints where
the live file is.

Check a file with `powerqueue priority check`. See what it does to one task
with `powerqueue priority explain ENG-123`.

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
| `## Models` | model tier per criticality |
| `## Jev` | Jev question and rubric |

Rules are bullet lines (`- ...` or `* ...`). Blank lines and non-bullet lines
inside a section are ignored, and HTML comments (`<!-- ... -->`, even across
lines) are stripped; there is no `#` line comment. Unknown sections, bullets
before the first `##`, a repeated `## Default` or `## Models` entry and unknown
Jev settings produce warnings, which `priority check` and `doctor` report with
line numbers. A bullet that cannot be parsed (unknown field, bad operator,
invalid regex, unknown tier) is an error.

## Conditions

A condition compares one task field with a value.

| Form | Meaning | Example |
|------|---------|---------|
| `field: value` | equals, case-insensitive; for `label`, membership in the label list (`=` and `==` are accepted too) | `label: incident` |
| `field != value` | not equals (for `label`: label absent) | `project != Sandbox` |
| `field ~ regex` | case-insensitive regex match (for `label`: any label matches) | `title ~ "^hotfix"` |
| `field > n` | numeric greater than (`priority`, `estimate` only) | `estimate > 8` |
| `field < n` | numeric less than (`priority`, `estimate` only) | `priority < 3` |

Join conditions with ` and `. All conditions in a rule must hold. There is no
`or`: write a second bullet instead.

```markdown
## Critical
- priority: urgent and label: customer
- label: incident
```

Quotes around values are optional; use them when a value contains spaces or
regex metacharacters.

## Fields

| Field | Type | Values |
|-------|------|--------|
| `label` | list of strings | any Linear label, or labels given with `add --label` |
| `priority` | enum / number | `urgent`, `high`, `normal` (`medium`), `low`, `none`, or Linear's 0–4 (0 = none, 1 = urgent) |
| `estimate` | number | Linear estimate points |
| `project` | string | Linear project name |
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
override is a *preference* handed to the budget policy, which may still
downgrade it when the tier is out of budget (it says so in `task explain`);
only `task model` and `add --model` set a hard `model_override`, and even
that is downgraded rather than ignored.

## Models

```markdown
## Models
- critical: fable
- high: opus
- normal: sonnet
- low: sonnet
```

Maps a criticality to a preferred tier (`fable`, `opus`, `sonnet`, `haiku`;
`critical = fable` also works). Missing levels leave the choice to the budget
policy: `normal` falls back to `budget.default_model`, `low` to
`budget.low_model`, and `critical`/`high` take the most capable eligible
tier. As with override models, this is a preference: the policy may pick a
cheaper tier when the preferred one is over budget or rate-limited.

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
4. **Model**: the CLI (`task model`, `add --model`) wins, then an
   `## Overrides` model, then `## Models` for the criticality, else none. All
   of these are handed to the budget policy as the preferred tier; it picks
   that tier when eligible, else the most capable eligible tier *below* it,
   and never upgrades above it (see [budget.md](budget.md)).

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
| model from Models | `model fable from ## Models` |

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

Pair with `budget.models.fable.min_criticality = "critical"` (the default) so
only critical tasks touch Fable early in the period.

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
