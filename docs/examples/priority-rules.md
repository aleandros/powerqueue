# Priority rules example

[Documentation](../README.md) · [Schema and migration](../priority.md)

Save this as `priority.toml`. Criticality is checked from critical to low;
all matching scoring rules add points. The daemon reloads valid edits, and
`powerqueue priority simulate --file draft.toml --reasons` previews a draft.

```toml
version = 1

[[criticality]]
level = "critical"
all = [{ field = "label", op = "eq", value = "incident" }]

[[criticality]]
level = "critical"
all = [
  { field = "priority", op = "eq", value = "urgent" },
  { field = "label", op = "eq", value = "customer" },
]

[[criticality]]
level = "low"
all = [{ field = "label", op = "eq", value = "chore" }]

[[scoring]]
delta = 40
all = [{ field = "label", op = "eq", value = "customer" }]

[[scoring]]
delta = -30
all = [{ field = "estimate", op = "gt", value = 8 }]

[[scoring]]
delta = 150
all = [{ field = "cycle", op = "eq", value = "active" }]

[[scoring]]
delta = -100
all = [{ field = "cycle", op = "eq", value = "future" }]

[[overrides]]
key = "ENG-123"
criticality = "critical"

[[overrides]]
key = "ENG-200"
models = ["opus", "gpt-6-astra"]

[[overrides]]
key = "ENG-300"
skip = true

[[model_rules]]
models = ["fable"]
all = [{ field = "label", op = "eq", value = "model/fable" }]

[models]
critical = ["fable", "gpt-6.1-sol"]
high = ["opus", "gpt-6-astra"]
normal = ["sonnet"]
low = ["sonnet"]
```

Model arrays list preferences in order. Task overrides precede the first
matching `model_rules` row, which precedes the criticality's model list.
The budget policy still controls provider eligibility and downgrade chains.
Qualified Linear labels distinguish `model/fable` from a loose `fable` label;
a bare `fable` condition matches both. The `cycle` rules above prefer the
active cycle. See the reference for all fields, defaults and precedence.

Keep optional prose in `PRIORITY.md`; it is not executable. Convert existing
Markdown rules with `powerqueue priority migrate --from PRIORITY.md --to
priority.toml`, then update the configured policy path.
