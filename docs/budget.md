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
| period budget | `period_weighted_tokens` | 80,000,000 weighted tokens |
| window budget | `window_weighted_tokens` | 12,000,000 weighted tokens |

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

With the defaults Fable may spend 20M weighted tokens per week, and only
`critical` tasks may use it early on. Shares of enabled tiers must sum to at
most 1.0 (`config validate` checks); a tier with `enabled = false` or share 0
is never chosen. Tasks of criticality `low` with no preference go to
`budget.low_model` (Sonnet), `normal` ones to `budget.default_model`, and
`critical`/`high` ones to the most capable eligible tier.

## The decision

`Policy::decide` runs for every task the scheduler wants to start. It is
pure: the same ledger, prediction and preference always give the same answer.

**Preference.** The scheduler passes one preferred tier: `task.model_override`
(set only by `task model <tier>` and `add --model`) if present, else the
PRIORITY.md model (`KEY: model = x` override, then `## Models` for the
criticality). The reasons say which: `model override: fable` or
`preferred by rules: fable`. Both are treated the same way below; the policy
may downgrade either and never upgrades above it.

**Per-tier verdict.** Every enabled tier with a share > 0 is checked in
order, most capable first (`Policy::verdict`); the first failing gate gives
the reason:

1. **Rate limit.** A tier on cooldown is out until it ends
   (`rate limited until <time>`).
2. **Criticality gate.** The task must be at least `min_criticality`, or the
   tier must be *relaxed*:
   - after `relax_after_fraction` of the period, if the tier's spend fraction
     (`period_weighted / period_budget`) is below the period's elapsed
     fraction (the tier is under-paced), one criticality level lower
     qualifies;
   - in the **end game** (elapsed ≥ `endgame_fraction`, default 0.8), two
     levels lower qualify regardless of pacing.
   Reason when blocked: `reserved for critical (relaxed to high); task is low
   (20% of its budget spent at 68% of the period)`.
3. **Tier budget.** `predicted × weight` must fit the tier's remaining share
   times `1 - safety_margin` (default 5% of what is left stays unspent).
4. **Overall period budget.** The calibrated period fraction plus this task's
   cost must stay below `1 - safety_margin`.
5. **Window.** `total_window_weighted + cost` must fit
   `window_weighted_tokens` (`window: 10000000 of 12000000 weighted tokens
   used, needs 3000000 more`).

An eligible tier reads `eligible; 20% of its budget spent at 68% of the
period` (or `eligible (relaxed to high); ...`).

**Choice.** With a preference: that tier if eligible, else the most capable
eligible tier *below* it (`preferred fable not eligible; downgraded to opus`).
Without one: `critical`/`high` take the most capable eligible tier, `normal`
takes `default_model` or the best eligible tier below it, `low` the same with
`low_model`.

**Throttle.** If nothing fits the task gets no model and a `retry_at`: the
earliest hint among the blocking tiers (rate-limit expiry, the next relaxation
or end-game boundary, the period end, or `WINDOW_RECHECK` = 15 minutes when
the window was the blocker), capped at the period end and never in the past.
The task state becomes `throttled` and `not_before` is set; `pick_next` takes
it again once that passes.

Several launches in one tick reserve their predicted cost in the in-memory
ledger so they do not all see the same headroom. The whole trail is stored
in the `task.starting` / `task.throttled` event and printed by
`powerqueue task explain`.

## Rate-limit cooldowns

Claude Code does not exit on a rate limit; it fires a `StopFailure` hook with
`error_type: rate_limit` (also `overloaded`, `usage_limit`, `quota`). The
daemon marks the session's tier exhausted for `rate_limit_cooldown_mins`
(default 30; at least one minute, never past the period end), logs
`budget.rate_limited` and puts the task in `throttled` with `not_before` at
the cooldown end. If the session is still alive when the cooldown passes the
task simply goes back to `running`; otherwise it is relaunched like any
throttled task. The state is persisted in the `kv` table under
`budget.rate_limits` (`RATE_LIMITS_KEY`) so a daemon restart does not forget
it. `powerqueue budget clear-limits` forgets it on purpose, for example after
`/usage` shows the window has reset.

## Calibration

powerqueue only sees its own sessions. Interactive work in another terminal
also drains the same allowance. Two commands feed outside knowledge in:

- `budget set-reset <when>` stores `period_anchor` so period boundaries are
  right.
- `budget set-observed 43%` records what `/usage` shows. The ledger stores the
  observed fraction, the time, and powerqueue's own measured fraction at that
  moment (`kv` key `budget.calibration`, `CALIBRATION_KEY`). From then on the
  period fraction used by the policy's overall-budget gate is
  `measured + (observed - measured)`, clamped to 0..2, so pacing accounts for
  usage it cannot see. A calibration taken in an earlier period is ignored;
  `budget show` says `calibration none this period` until you run it again.
  Re-run it whenever the numbers drift.

If `/usage` shows you are consistently ahead of powerqueue's estimate, lower
`period_weighted_tokens`; if you never get close to the limit, raise it.

## The estimator

`Estimator` learns from `task_usage_summaries` (every task that has attempts
or usage): only `completed` and `failed` tasks with weighted usage above zero
become samples. Each sample records criticality, estimate points, labels,
weighted tokens (before the tier weight) and wall-clock seconds. Failed
samples count 1.5× (`FAILED_PENALTY`) because they burned budget without a
result.

Prediction for a new task (`src/budget/estimator.rs`):

1. Find the most specific bucket that has samples: same estimate points, then
   same criticality, then same first label (case-insensitive), else all
   samples (`global`).
2. With at least 3 samples (`TRUST_N`) use the bucket's median as is.
   Smaller buckets are shrunk toward the global median with a pseudo-count of
   3 (`SHRINK_K`): `(n × bucket + 3 × global) / (n + 3)`.
3. Report `weighted_tokens`, `wall_secs`, a `confidence` of `n / 10` capped
   at 1 (0 = no history) and the `basis`, e.g. `estimate=3 (n=4)` or
   `label=incident (n=1, shrunk toward global median of 7)`.

With no history at all the guess is 500,000 weighted tokens and 1,800
seconds (`DEFAULT_WEIGHTED_TOKENS`, `DEFAULT_WALL_SECS`), roughly one focused
half-hour Sonnet session. `powerqueue budget estimate ENG-123` shows the
prediction; `budget estimate` without a task prints the sample count and
accuracy. The accuracy is the leave-one-out mean absolute percentage error
over completed samples, available from four of them; `doctor` warns below 5
samples or above 60% error.

## Worked examples

Defaults throughout: period 168h anchored Monday 00:00, Fable share 0.25 → 20M
weighted budget, window 12M.

### Monday morning, incident ticket

- Elapsed fraction 0.05. Ledger: Fable 0 of 20M spent, window empty.
- Ticket `ENG-900`, label `incident` → `## Critical` → criticality `critical`,
  `## Models` says `critical: fable`. Preferred tier: Fable.
- Estimator: bucket `label=incident (n=3)`, prediction 600k weighted tokens.
- Fable: no cooldown; `critical` meets `min_criticality`; 600k × 5 = 3M fits
  the 19M available (20M × 0.95), pushes the period to 4% and fits the 12M
  window.
- Decision: **Fable**. Reasons: `period 5% elapsed, 0% spent; window 0% used;
  predicted cost 600000 weighted tokens (label=incident (n=3))`,
  `fable: eligible; 0% of its budget spent at 5% of the period`, ...,
  `preferred by rules: fable`, `preferred fable is eligible`.

Had the window already held 10M, the window gate would fail for Fable
(`fable: window: 10000000 of 12000000 weighted tokens used, needs 3000000
more`); Opus (600k × 3 = 1.8M) fits, and the task runs on Opus with the
reason `preferred fable not eligible; downgraded to opus`.

### Friday evening, backlog chore

- Friday 18:00 → elapsed 0.68. Fable has spent 4M of 20M (20%, under-paced).
- Ticket `ENG-912`, label `chore` → `## Low` → `low`; the template's
  `## Models` says `low: sonnet` (without that line `budget.low_model`
  applies, which is Sonnet too).
- Fable: `low` is below `critical`; relaxed since 0.68 ≥ 0.5 and Fable is
  under-paced, so one level lower, `high`, qualifies. `low` is three levels
  below. Not eligible.
- Sonnet: `min_criticality = low` → eligible.
- Decision: **Sonnet**. Reasons: `fable: reserved for critical (relaxed to
  high); task is low (20% of its budget spent at 68% of the period)`,
  `sonnet: eligible; ...`, `preferred sonnet is eligible`.

### Sunday afternoon, same chore

- Elapsed 0.93 ≥ `endgame_fraction` 0.8. Fable still at 20%.
- End game opens Fable two levels lower, down to `normal`. A `normal` task
  predicted to cost 1M (5M on Fable) fits the 15.2M available (16M remaining
  × 0.95), so a `normal` task would get Fable now.
- `low` is still outside. The chore stays on Sonnet.

To let chores soak up leftover Fable on weekends, set
`budget.models.fable.min_criticality = "high"` or add a scoring rule that
raises the chore to `normal` on Fridays (edit PRIORITY.md; it reloads live).

## Tuning with `doctor`

`powerqueue doctor` has an *algorithm* category (the anchor check sits under
*configuration*). Each finding comes with a concrete suggestion
(`src/doctor.rs`):

| Check | Warns when | Hint |
|-------|------------|------|
| budget anchor | `period_anchor` not set | `budget set-reset <time from /usage>` |
| cost estimator | fewer than 5 samples, or leave-one-out error above 60% | let tasks finish; add Linear estimates and consistent labels so buckets form |
| fable reservation | under-used: > 70% of the period gone with < 30% of Fable's share spent | lower `models.fable.relax_after_fraction` or `min_criticality` |
| fable reservation | over-paced: spent fraction more than 25 points ahead of elapsed | raise `models.fable.min_criticality` or lower its share |
| window pressure | more than 90% of the window budget spent | lower `scheduler.max_concurrent` or raise `window_weighted_tokens` if `/usage` shows headroom |
| throttling | more than 10 `task.throttled` events in 24 h | raise `period_weighted_tokens` / `window_weighted_tokens` or lower expensive shares |
| crash rate | `session.crashed` over `session.launched` above 30% in 7 days | check `claude.permission_mode` and `repo.setup`; raise `stale_session_secs` if work is just slow |
| idle / attention rate | `session.nudged` + `task.needs_attention` + `task.blocked` + `session.permission_prompt` events over launches above 30% | tighten the completion protocol (always run `task complete`) or use a less interactive permission mode |

There is no staleness check for the calibration; `budget show` tells you
whether one exists for the current period. `doctor --json` includes the
detail text of each finding so you can track it.
