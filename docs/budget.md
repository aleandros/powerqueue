# Budget pacing

A Claude subscription gives a weekly allowance plus a rolling 5-hour window.
Both are shared by every model, and Fable is the most capable and the scarcest.
powerqueue wants critical work on Fable immediately, routine work on cheaper
tiers, and no Fable capacity wasted at the end of the week. There is no API for
the remaining allowance, so powerqueue measures what it spends, lets you
calibrate against `/usage`, and treats rate-limit errors as a hard signal.

The code lives in `src/budget/`: `period.rs` (where we are in the period),
`ledger.rs` (what was spent), `estimator.rs` (what a task will cost) and
`policy.rs` (the decision). Config keys are in `[budget]` of `config.toml`.

## Periods and windows

| Concept | Config | Default |
|---------|--------|---------|
| period length | `period_hours` | 168 (one week) |
| period start | `period_anchor` | none; set with `budget set-reset` |
| rolling window | `window_hours` | 5 |
| period budget | `period_weighted_tokens` | 60,000,000 weighted tokens |
| window budget | `window_weighted_tokens` | 4,000,000 weighted tokens |

Periods are half-open intervals `[start, end)` repeating every `period_hours`
before and after the anchor. The current period is the one containing now; its
**elapsed fraction** goes from 0.0 at start to 1.0 at end. The window is always
`[now - window_hours, now)`.

Without an anchor the clock assumes Monday 00:00 UTC of the current week. That
is wrong for most accounts but stable, and `doctor` nags until you run:

```sh
powerqueue budget set-reset "2026-10-05T14:00:00Z"   # the reset time /usage shows
powerqueue budget set-reset "in 3d4h"                # or relative to now
```

## Weighted tokens

Subscription limits do not count raw tokens. powerqueue approximates them with
`TokenUsage::weighted` (`src/domain.rs`):

```text
weighted = input_tokens
         + 1.25 × cache_creation_input_tokens
         + 0.10 × cache_read_input_tokens
         + 5.00 × output_tokens
```

Each tier then has a cost **weight** relative to Sonnet
(`budget.models.<tier>.weight`; defaults Fable 5, Opus 3, Sonnet 1, Haiku 0.2):

```text
tier_weighted = weighted × tier.weight
```

Usage comes from the Claude Code transcript (one row per API message id, so
multi-block responses count once). The ledger sums rows per tier for the
current period and window and reports:

- `period_weighted` / `window_weighted` per tier,
- `period_budget = share × period_weighted_tokens` per tier,
- totals across tiers against `period_weighted_tokens` and `window_weighted_tokens`.

`powerqueue budget show` prints this table.

## Shares and minimum criticality

Each tier gets a slice of the period and a floor on who may use it:

| tier | `share` | `min_criticality` | `relax_after_fraction` | `weight` |
|------|--------:|-------------------|-----------------------:|---------:|
| fable | 0.25 | critical | 0.5 | 5.0 |
| opus | 0.35 | high | 0.3 | 3.0 |
| sonnet | 0.35 | low | 0.0 | 1.0 |
| haiku | 0.05 | low | 0.0 | 0.2 |

With the defaults Fable may spend 15M weighted tokens per week, and only
`critical` tasks may use it early on. Shares of enabled tiers must sum to at
most 1.0 (`config validate` checks). Tasks of criticality `low` go to
`budget.low_model` (Sonnet) unless rules say otherwise; tasks with no preference
go to `budget.default_model`.

## The decision

`Policy::decide` runs for every task the scheduler wants to start. Rules, in
order:

1. **Preference.** `task.model_override` (from `task model`, `add --model` or a
   PRIORITY.md override) or the `## Models` entry for the task's criticality
   picks a *preferred* tier. The policy may still downgrade it and says so in
   the reasons.
2. **Rate limits.** A tier that reported `rate_limit` is unavailable until its
   cooldown ends.
3. **Window.** The rolling window must have room for the predicted cost of the
   task on that tier.
4. **Eligibility.** A tier is eligible when the task's criticality is at least
   `min_criticality`, or when the tier is *relaxed*:
   - after `relax_after_fraction` of the period, if the tier's spend fraction
     (`period_weighted / period_budget`) is below the period's elapsed fraction
     (the tier is under-paced), one criticality level lower qualifies;
   - in the **end game** (elapsed ≥ `endgame_fraction`, default 0.8), two
     levels lower qualify as long as the predicted cost fits the tier's
     remaining budget after the **safety margin** (`safety_margin`, default 5%
     of the period budget kept unspent).
5. **Choice.** Among eligible tiers prefer the preferred one, else the most
   capable.
6. **Throttle.** If nothing is eligible the task gets no model and a
   `retry_at` equal to the earlier of the window roll-over and the period end.
   The task state becomes `throttled`.

The whole trail is stored on the task and printed by `powerqueue task explain`.

## Rate-limit cooldowns

Claude Code does not exit on a rate limit; it fires a `StopFailure` hook with
`error_type: rate_limit` (or `overloaded`). The daemon marks the session's
tier exhausted for `rate_limit_cooldown_mins` (default 30), or until the window
or period resets if that comes sooner, and puts the task in `throttled`. The
state is persisted in the `kv` table under `budget.rate_limits` so a daemon
restart does not forget it. `powerqueue budget clear-limits` forgets it on
purpose, for example after `/usage` shows the window has reset.

## Calibration

powerqueue only sees its own sessions. Interactive work in another terminal
also drains the same allowance. Two commands feed outside knowledge in:

- `budget set-reset <when>` stores `period_anchor` so period boundaries are
  right.
- `budget set-observed 43%` records what `/usage` shows. The ledger stores the
  observed fraction, the time, and powerqueue's own measured fraction at that
  moment (`kv` key `budget.calibration`). From then on the period fraction used
  by the policy is the measured fraction plus the offset between observed and
  measured, so pacing accounts for usage it cannot see. Re-run it whenever the
  numbers drift; `doctor` warns when the calibration is stale.

If `/usage` shows you are consistently ahead of powerqueue's estimate, lower
`period_weighted_tokens`; if you never get close to the limit, raise it.

## The estimator

`Estimator` learns from `task_usage_summaries` (every task with a session):
completed tasks, and failed ones flagged as such, with usage above zero. Each
sample records criticality, estimate points, labels, weighted tokens and
wall-clock seconds.

Prediction for a new task:

1. Find the bucket with the most specific match that has samples: same
   estimate points, then same criticality, then shared labels.
2. Shrink the bucket's median toward the global median; the fewer samples in
   the bucket, the stronger the pull.
3. Report `weighted_tokens`, `wall_secs`, a `confidence` in 0..1 (0 = no
   history) and the `basis` it came from, e.g. `estimate=3 (n=4)`.

With no history at all the guess is 1,500,000 weighted tokens and 1,800
seconds, roughly one focused Sonnet session. `powerqueue budget estimate
ENG-123` shows the prediction; `budget estimate` without a task dumps the
samples. `doctor` reports the estimator's leave-one-out mean absolute
percentage error and the sample count.

## Worked examples

Defaults throughout: period 168h anchored Monday 00:00, Fable share 0.25 → 15M
weighted budget, window 4M.

### Monday morning, incident ticket

- Elapsed fraction 0.05. Ledger: Fable 0 of 15M spent, window empty.
- Ticket `ENG-900`, label `incident` → `## Critical` → criticality `critical`,
  `## Models` says `critical: fable`. Preferred tier: Fable.
- Estimator: bucket `label=incident (n=3)`, prediction 600k weighted tokens.
- Rule 2: no cooldown. Rule 3: 600k × 5 = 3M fits in the 4M window.
- Rule 4: `critical` ≥ Fable's `min_criticality` → eligible outright.
- Decision: **Fable**. Reasons: `preferred fable (PRIORITY.md Models)`,
  `fable eligible: critical >= critical`, `window 3.0M/4.0M`.

Had the window already held 2M, rule 3 would fail for Fable; Opus
(600k × 3 = 1.8M) might fit, and the task would run on Opus with the reason
`fable: window full, downgraded`.

### Friday evening, backlog chore

- Friday 18:00 → elapsed 0.68. Fable has spent 4M of 15M (27%, under-paced).
- Ticket `ENG-912`, label `chore` → `## Low` → `low`; model from
  `budget.low_model` = Sonnet.
- Rule 4 for Fable: `low` is below `critical`; relaxed since 0.68 > 0.5 and
  Fable is under-paced, so one level lower, `high`, qualifies. `low` is three
  levels below. Not eligible.
- Sonnet: `min_criticality = low` → eligible.
- Decision: **Sonnet**. Reasons: `fable not eligible: low < high (relaxed)`,
  `sonnet eligible`.

### Sunday afternoon, same chore

- Elapsed 0.93 ≥ `endgame_fraction` 0.8. Fable still at 27%.
- End game opens Fable two levels lower, down to `normal`. A `normal` task
  predicted to cost 1M (5M on Fable) fits the 11M remaining after the 0.75M
  margin, so a `normal` task would get Fable now.
- `low` is still outside. The chore stays on Sonnet.

To let chores soak up leftover Fable on weekends, set
`budget.models.fable.min_criticality = "high"` or add a scoring rule that
raises the chore to `normal` on Fridays (edit PRIORITY.md; it reloads live).

## Tuning with `doctor`

`powerqueue doctor` has an *algorithm* category. Each finding comes with a
concrete suggestion:

| Finding | Hint |
|---------|------|
| period anchor not set | `budget set-reset <time from /usage>` |
| calibration older than a few days | `budget set-observed <percent>` |
| estimator accuracy poor or few samples | nothing to change; it improves with history. Give tickets estimates so buckets form |
| Fable under-reserved: spent fraction far below elapsed fraction week after week | lower `models.fable.relax_after_fraction`, raise `endgame_fraction` release, or loosen `min_criticality` to `high` |
| Fable over-reserved: critical tasks throttled while Fable has budget | raise `models.fable.share`, lower other shares |
| throttling frequent | after calibrating, raise `period_weighted_tokens` / `window_weighted_tokens`, or lower `scheduler.max_concurrent` |
| crash rate high | check `claude.permission_mode` and `repo.setup`; see [troubleshooting.md](troubleshooting.md) |
| idle rate high | prompts end without the completion marker; tighten `instructions` in `.powerqueue.toml` |

`doctor --json` includes the numbers behind each finding so you can track them.
