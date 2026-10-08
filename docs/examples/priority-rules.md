# Priority rules example

[Documentation](../README.md) · [Rules reference](../priority.md)

Rules live in a Markdown file so they stay readable anywhere. The daemon
watches the file and re-scores the queue when it changes, so edits apply
without a restart; `powerqueue priority simulate` shows the effect of an edit
(or of a draft file) before you save it. Sections are `##`
headings; rules are bullets. A ticket's criticality is the first section
(Critical → High → Normal → Low) with a matching rule.

```markdown
## Critical
- label: incident
- priority: urgent and label: customer

## Low
- label: chore

## Scoring
- +40 if label: customer
- -30 if estimate > 8
- +150 if cycle: active
- -100 if cycle: future

## Overrides
- ENG-123: critical
- ENG-200: model = opus | gpt-6-astra
- ENG-300: skip

## Models
- if label: model/fable: fable
- critical: fable | gpt-6.1-sol
- high: opus | gpt-6-astra
- normal: sonnet
- low: sonnet
```

The file is re-read whenever it changes. `## Models` and `KEY: model = ...`
are preference lists handed to the budget policy (`|`-separated, most wanted
first; alternatives may belong to other enabled providers); the policy tries
each one through its provider's downgrade chain and falls back to
`budget.provider_order` when none fits. Only `task model` and `add --model`
set a hard override, and even that is downgraded (within its provider) when
the model is out of budget. An `if <conditions>: <models>` row under
`## Models` picks models by label (or any condition) and beats the
criticality row; `priority explain` shows `model: fable (if label:
model/fable)`. Linear child labels are stored as `parent/name`
(`model/fable`), so they are distinct from a loose `fable` label; an
unqualified `label: fable` still matches both. Conditions can use `label`, `priority`,
`estimate`, `project`, `cycle` (`active`, `next`, `past`, `future`),
`cycle_number`, `team`, `title`, `description`, `source` and `key`; the two
`cycle` lines above are the "prefer the current cycle" idiom. Full grammar,
evaluation order and idioms: [docs/priority.md](../priority.md).
