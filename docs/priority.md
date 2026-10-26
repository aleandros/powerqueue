# priority.toml: typed executable policy

`priority.toml` determines criticality, score, skip decisions and preferred
models. `PRIORITY.md` may contain free-form intent for people and agents, but
Markdown prose alone is **not executable**. The daemon never evaluates Markdown.
Use `tune` to translate that intent and an instruction into a validated TOML
draft for review. `init` generates both a free-form `PRIORITY.md` template and
a typed `priority.toml` template. The old Markdown grammar is used only by the
explicit migration command.

## Schema (version 1)

```toml
version = 1
# Optional; the built-in fallback is normal.
default_criticality = "normal"

[[criticality]]
level = "critical"
all = [{ field = "label", op = "eq", value = "incident" }]

[[criticality]]
level = "high"
all = [
  { field = "priority", op = "eq", value = "high" },
  { field = "label", op = "ne", value = "chore" },
]

[[scoring]]
delta = 40
all = [{ field = "source", op = "eq", value = "github" }]

[[scoring]]
delta = -30
all = [{ field = "estimate", op = "gt", value = 8 }]

[[model_rules]]
models = ["fable", "gpt-6.1-sol"]
all = [{ field = "label", op = "eq", value = "model/fable" }]

[[overrides]]
key = "ENG-123"
criticality = "high"
score_delta = 100
models = ["opus", "gpt-6.1-sol"]

[[overrides]]
key = "ENG-200"
skip = true

[models]
critical = ["fable", "gpt-6.1-sol"]
high = ["opus"]
normal = ["sonnet"]
low = ["sonnet"]

[jev]
enabled = false
question = "How important is it to ship this ticket this week?"
levels = ["later", "soon", "now"]
```

`version` is required. Other top-level keys are optional; absent rule lists and
model maps are empty. Unconfigured policies use normal criticality, no custom
scores/skips/model preferences, and disabled Jev. `init` writes the richer
shipped template (incident/security/urgent critical; customer/high high;
normal priority normal; chore/docs low).
The template also adds 250 points for tasks with both high priority and a `bug`
label, so older ordinary high-priority tasks cannot win through age alone.
Existing policy files are not rewritten when powerqueue is upgraded.

All tables reject unknown keys. Duplicate TOML keys, invalid criticalities,
unknown models, duplicate model preferences, case-insensitive duplicate task
keys, empty condition/model lists and non-finite numbers fail validation.
`priority check` reports source lines; semantic errors also identify the rule
array and index (zero-based). Jev requires a nonempty question and 2–10 nonempty
levels. Every override must set at least one action. `skip` defaults to false;
other override actions default to absent.

### Conditions

Conditions are data: `{ field = ..., op = ..., value = ... }`. `all` is a
nonempty conjunction (AND). Separate criticality rules supply alternatives
(OR); no expression mini-language is parsed from values.

| Field | Meaning |
|---|---|
| `label` | membership; qualified Linear labels such as `model/fable` work; bare child names retain legacy matching |
| `priority` | Linear priority: none/no priority/0, urgent/1, high/2, normal/medium/3, low/4 |
| `estimate` | numeric estimate |
| `cycle_number` | numeric Linear cycle number |
| `cycle` | active, next, past, future |
| `project` | source Linear project name, **not** execution-project ID |
| `team` | Linear team key |
| `title`, `description`, `key` | task text |
| `source` | linear, github, manual |
| `repository` | GitHub source owner/repo; absent for Linear/manual tasks |

`eq` and `ne` use case-insensitive text equality or numeric equality. `matches`
is a case-insensitive Rust regex, supplied as a string (TOML literal strings
are convenient for backslashes). `gt` and `lt` accept TOML numbers and only
numeric fields (`priority`, `estimate`, `cycle_number`). Equality for numeric
fields also accepts numeric strings and priority names for migration fidelity.
Missing optional fields fail every predicate except `ne`, which succeeds.
Labels use the existing qualified/bare-child equality and regex behavior.
Strings containing colons, `and`, quotes or pipes are ordinary values, not
condition separators. A regex with `|` keeps its normal regex meaning.

### Ordering and precedence

1. Match a task override by case-insensitive key. Its criticality replaces
   classification, its score delta adds to scoring, its models take precedence,
   and `skip = true` pauses the task. Removing a skip lets the daemon re-evaluate
   it (dependency and user-pause rules still apply).
2. Otherwise classify by **critical → high → normal → low**, regardless of
   the order different levels appear in the file. Within a level, the first
   matching rule wins. With no match, use `default_criticality`.
3. Begin at the criticality base score: critical 1000, high 500, normal 100,
   low 10. Add **every** matching scoring delta in file order, then the task
   override delta. Add the configured Jev contribution and waiting-age boost
   (capped at 200), as before.
4. Model preference is task override → first matching `model_rules` row in
   file order → `models.<effective criticality>` → budget defaults.

Queue order is highest score first, then criticality, then oldest creation time.
A small bonus is not a guarantee: +40 for bugs loses to an ordinary task with
more than 40 points of extra age. To keep high-priority bugs ahead of ordinary
high-priority work, use one rule for the category rather than ticket overrides:

```toml
[[scoring]]
delta = 250
all = [
  { field = "priority", op = "eq", value = "high" },
  { field = "label", op = "eq", value = "bug" },
]
```

This exceeds the 200-point age cap. Other scoring rules, overrides and enabled
Jev scores still add, so check their combined effect with `simulate --reasons`.
When tuning repository-owned policy, select its execution project with
`tune --project PROJECT`; tuning the machine policy does not replace a project's
own policy. Ticket names in a general ranking complaint are examples, not a
reason to maintain an override for each task.

The existing budget/provider eligibility, reservations, fallback, downgrade
chains and CLI `task model` behavior are unchanged. Policy model lists are
preferences, not permission to exceed budgets or enable a disabled provider.
`explain` shows decisions and source locations; `simulate --reasons` shows
queue changes and budget decisions without writing state.

## Machine and execution-project policies

The machine default is `<config>/priority.toml`, or `[priority].file` in
`config.toml`. A registered project's `[projects.ID.priority]` replaces the
machine priority settings when present; omitted settings in that block use
`PriorityConfig` defaults. Without the block it inherits machine settings.
The project's `.powerqueue.toml` `priority_file` takes precedence over either
file selection and resolves relative to its checkout. Use absolute paths for
`priority.file` and `projects.ID.priority.file` (relative filesystem paths use
the process working directory). Policies replace the
whole selected document; rules from different files are not concatenated.
Machine budgets and total concurrency remain machine-wide.

With `repo.overrides_from = "default-branch"`, a repository-relative
`priority_file` is read from the same commit as `.powerqueue.toml`. Editing the
working copy does not affect execution until that commit is selected. Absolute
paths are read from disk. `project list --json` and `priority path --project ID`
show effective paths. `priority explain TASK` selects the task's execution
project automatically; live simulation evaluates each task using its own
project policy. `--project ID` filters simulation. `--file` applies a draft
policy to the selected tasks while keeping machine budget rules.

## Migration from Markdown

Stop scheduling while switching files, then convert each global or project
policy explicitly:

```sh
powerqueue pause
powerqueue priority migrate --from PRIORITY.md --to priority.toml
powerqueue priority check --file priority.toml
powerqueue priority simulate --file priority.toml --reasons
# Update [priority].file, [projects.ID.priority].file, or priority_file
# in .powerqueue.toml. Commit both repository-owned changes when applicable.
# Restart the daemon or use Reload in the dashboard.
powerqueue resume
```

Migration never overwrites either file or changes configuration. Keep the
Markdown for optional intent/history. If the old file was at the default
`<config>/PRIORITY.md`, the new default is `<config>/priority.toml`; no path
setting is necessary after migration. If only the legacy default exists,
loading and initialization report the migration command rather than silently
using defaults. Explicit Markdown policy paths must also be migrated.

The converter uses the actual legacy parser, including `*` bullets, case
variants, `=`/`==`, comments, repeated sections/settings, optional scoring `if`,
conditional-model colon splitting, model aliases and Jev aliases. It preserves
first default, last per-criticality model row, last override criticality/model,
additive override deltas, and first conditional model match. It reports the
legacy warnings for ignored/duplicate content. Explanations now refer to TOML
locations; evaluation equivalence is checked on generated synthetic tasks.

Intentional changes: unknown keys are errors rather than ignored warnings;
duplicate override keys must be combined; duplicate models must be removed;
non-finite numbers must be repaired. Migration consolidates supported duplicate
settings automatically. The old parser split `and` even inside quotes; malformed
legacy conditions are rejected, not guessed. TOML now supports literal `and`
and other punctuation inside values. Arbitrary legacy prose has no rule
semantics and migrates to defaults, with that limitation reported by the command.

## Commands and reload

```sh
powerqueue priority show --project api
powerqueue priority check --project api
powerqueue priority path --project api
powerqueue priority edit --project api
powerqueue priority explain ENG-123
powerqueue priority simulate --project api --reasons
powerqueue priority simulate --file draft.toml --no-budget --all
powerqueue tune "customer tickets should rank above chores" --scope priority --dry-run
```

`show`, `check`, `edit` and `path` select the machine policy when `--project`
is omitted. JSON is available through the global `--json` option. `edit` uses
`$VISUAL`, then `$EDITOR`, then `vi`. `tune --project ID` reads the selected
policy and adjacent `PRIORITY.md` (or
`--intent PATH`), edits TOML drafts and shows the diff and simulated effects
before apply. It records the intent snapshot; tune and doctor report subsequent
prose changes without changing scheduling. Repository policies read from a
committed branch produce working-copy changes for review and commit under that
same workflow. See [commands](commands.md) for apply, undo and stale-input checks.

File changes reload when `priority.live_reload = true`. Project policies are
polled on ticks; single-repository policies are watched, and committed policies
are polled through git. Invalid, unreadable or removed policies keep the last
valid policy and log an error/warning; `doctor` checks the current source and
reports repair/migration instructions. With no valid policy on first load,
invalid policies prevent launches for that project. A genuinely absent initial
policy uses built-in defaults. Last-valid policies are in memory; after a daemon
restart, invalid files must be repaired before that project can launch.
