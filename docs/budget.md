# Budget pacing

A Claude subscription gives a weekly allowance plus a rolling 5-hour window.
Both are shared by every model, and Fable is the most capable and the scarcest.
powerqueue wants critical work on Fable immediately, routine work on cheaper
tiers, and no Fable capacity wasted at the end of the week. It measures what
it spends, reads what the providers report about their own allowance
(usage probes, below) and treats those readings as the truth, learns the
exchange rate between its own token counts and the provider's percentages
(see [Learning the rate](#learning-the-rate)), and treats rate-limit errors
as a hard signal.

The code lives in `src/budget/`: `period.rs` (where we are in the period),
`ledger.rs` (what was spent; one `Ledger` per enabled provider in `Ledgers`),
`probe.rs` (what a provider reports about its own allowance), `probes/` (the
real probes per provider), `estimator.rs` (what a task will cost) and
`policy.rs` (the decision). Budgets are per provider: the shared knobs live
in `[budget]` of `config.toml`, everything about one subscription in
`[budget.providers.<provider>]`. Most of this page uses the Claude provider
as the example; Codex and Gemini have the same shape with their own defaults
(see [Providers](#providers) and the README) and are experimental.

```toml
[budget]
provider_order = ["claude", "codex", "gemini"]
default_model = "sonnet"
low_model = "sonnet"
safety_margin = 0.05
endgame_fraction = 0.8
probe_interval_mins = 15

[budget.providers.claude]
enabled = true
period_hours = 168
period_anchor = "2026-10-05T14:00:00Z"   # optional; `budget set-reset`
window_hours = 5                         # 0 = no window
period_weighted_tokens = 80000000
window_weighted_tokens = 12000000
rate_limit_cooldown_mins = 30

[budget.providers.claude.models.fable]
rank = 10
share = 0.25
min_criticality = "critical"
relax_after_fraction = 0.5
weight = 5.0
enabled = true
# opus (rank 20), sonnet (30), haiku (40) likewise
```

Files from before this layout (`budget.period_hours`, `[budget.models.fable]`)
still load: the keys are moved under `budget.providers.claude` on read and
`doctor` notes the move.

## Periods and windows

| Concept | Config (`[budget.providers.claude]`) | Default |
|---------|--------|---------|
| period length | `period_hours` | 168 (one week) |
| period start | `period_anchor` | none; set with `budget set-reset` or learned by a probe |
| rolling window | `window_hours` | 5 (`0` = no window: window checks always pass) |
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

Each model then has a cost **weight** relative to Sonnet
(`budget.providers.claude.models.<model>.weight`; defaults Fable 5, Opus 3,
Sonnet 1, Haiku 0.2; a model that is not in the config counts with weight 1):

```text
tier_weighted = weighted × tier.weight
```

Usage comes from the Claude Code transcript (one row per API message id, so
multi-block responses count once). The ledger sums rows per tier for the
current period and window and reports:

- `period_weighted` / `window_weighted` per tier,
- `period_budget = share × period_budget` per tier, where the ledger's
  `period_budget` is the *effective* one: learned from the provider's
  readings when possible, else `period_weighted_tokens`,
- totals across tiers against the effective period and window budgets.

`powerqueue budget show` prints this table. The weights are a guess at how
the provider meters usage; they are wrong by some factor (for Claude Code
sessions, which re-read a large context from cache on every turn, by a
large one), which is why the configured budgets are only a starting point
and the rate is learned.

## Shares and minimum criticality

Each model gets a slice of the period, a floor on who may use it and a
`rank` that orders the provider's models from most to least capable (the
downgrade chain is "next higher rank of the same provider"):

| model | `rank` | `share` | `min_criticality` | `relax_after_fraction` | `weight` |
|------|-------:|--------:|-------------------|-----------------------:|---------:|
| fable | 10 | 0.25 | critical | 0.5 | 5.0 |
| opus | 20 | 0.35 | high | 0.3 | 3.0 |
| sonnet | 30 | 0.35 | low | 0.0 | 1.0 |
| haiku | 40 | 0.05 | low | 0.0 | 0.2 |

With the defaults Fable may *lend* 20M weighted tokens per week (25% of the
configured 80M) to work it is not reserved for, and only `critical` tasks
may use it early on. A share caps borrowed use only: work a model is
reserved for (`task.criticality` at or above its `min_criticality`) is
limited by the whole allowance, never by the share, so an empty Fable share
cannot stop a critical task. Shares of enabled tiers must sum to at most 1.0
(`config validate` checks); a tier with `enabled = false` or share 0 is
never chosen. Tasks of criticality `low` with no preference go to
`budget.low_model` (Sonnet), `normal` ones to `budget.default_model`, and
`critical`/`high` ones to the most capable eligible tier.

## Providers

Each enabled provider (`budget.providers.<p>.enabled`) has its own ledger:
its own period clock and anchor, window, learned rate, observed usage and
model table. Usage rows count against the provider that runs the model
(`ModelTier::provider()`: `fable|opus|sonnet|haiku` → claude, `gpt-*` →
codex, `gemini-*` → gemini).

| provider | CLI | default models (rank) | window | probe |
|----------|-----|-----------------------|--------|-------|
| `claude` | `claude` | fable (10), opus (20), sonnet (30), haiku (40) | 5 h | the session's status line |
| `codex` | `codex` | gpt-6.1-sol (10), gpt-6-astra (20), gpt-6-luna (30) | 5 h (Pro-family plans have none: set `window_hours = 0`) | `codex app-server` → `account/rateLimits/read` |
| `gemini` | `agy` (Antigravity) | gemini-3-pro (10), gemini-3-flash (20) | 5 h | `agy -p /usage --output-format json` (experimental) |

`budget.provider_order` (default `["claude", "codex", "gemini"]`) is the
fallback order between enabled providers; enabled providers it does not list
come after it. A provider with `window_hours = 0` has no window check unless
its readings report one (Claude's status line always does).

## Usage probes and observed anchors

A probe asks a provider how much of its allowance is used, without spending
any of it. The daemon runs the probes of every enabled provider once at
start and then every `budget.probe_interval_mins` (default 15; `0` turns
them off) on a background thread, so a slow CLI never delays scheduling
(each probe is killed after 15 s). `powerqueue budget probe [--provider P]`
runs them now and prints what came back.

- **claude**: no extra call. Each task's `settings.json` sets a
  `statusLine` command (`powerqueue hook … --event StatusLine`). Claude Code
  runs it after responses with `rate_limits.five_hour` / `seven_day`
  (`used_percentage`, `resets_at`) on stdin — only for claude.ai Pro/Max
  logins and only after the first response. The hook stores them and prints
  a short status line (`pq sonnet 5h 75% · 7d 89%`) that you see when you
  attach. The probe reads back the latest one.
- **codex**: spawns `codex -s read-only -a never app-server`, sends
  `initialize`, `initialized` and `account/rateLimits/read`, and maps each
  bucket by its length: at most 600 minutes is the window, longer is the
  period. `ordinaryUsageAllowed: false` means blocked.
- **gemini**: runs `agy -p "/usage" --output-format json` and reads the
  `gemini-5h` / `gemini-weekly` buckets (`remaining_fraction`; `disabled`
  means blocked). The output shape comes from community reports; anything
  unexpected is ignored.

What a probe learns is stored in kv `budget.observed.<provider>` (latest
wins) and appended to kv `budget.observations.<provider>` (the reading
history, thinned to one per minute for the last two hours and one per ten
minutes before that, nothing older than nine days). It is used in three
ways:

1. **Truth.** The reported period and window percentages are where the
   ledger's fractions start; only usage recorded after the reading is added
   on top, converted at the effective budget (`period_fraction = used +
   spent_since_observation / period_budget`). `budget set-observed 43%`
   stores a manual reading the same way. The history teaches the rate (see
   [Learning the rate](#learning-the-rate)).
2. **Anchor.** The reported weekly reset (`period_resets_at`) moves the
   period so it ends there; `budget show` then says `anchor learned from the
   provider` and you never need `budget set-reset`.
3. **Cooldown.** `blocked`, or a window or period at 100%, keeps the whole
   provider off the candidate list until the matching reset (`claude reports
   its allowance blocked; skipped until the reset at …`). An exhausted
   reading without a reset time counts for an hour.

`budget show` prints each provider's observed line with its age (`observed
window 17% · period 40% (resets …) (3m ago)`), the anchor source (`config`,
`learned from the provider`, or a warning when neither is known) and the
last probe failure if there was one. Probe results are logged as
`budget.probe` events: info when the reading changes, a warning when a probe
fails (at most once an hour per provider).

## The decision

`Policy::decide` runs for every task the scheduler wants to start. It is
pure: the same ledgers, prediction and preferences always give the same
answer.

**Candidates.** Every enabled model with a share > 0 of every enabled
provider: providers in `provider_order`, each provider's models most capable
(lowest `rank`) first.

**Preferences.** In this order, the first that exists:

1. `task.model_override` (set only by `task model <model>` and `add
   --model`): a *hard* override. Reason: `model override: gpt-6-astra`.
2. The PRIORITY.md preference list (`KEY: model = x` override, then `##
   Models` for the criticality), most wanted first. Reason: `preferred by
   rules: fable | gpt-6.1-sol`.
3. The criticality default: `budget.default_model` for `normal`,
   `budget.low_model` for `low`, none for `critical`/`high`.

Each preferred model is tried through its own downgrade chain (the same
provider's models with a higher rank), in list order; the first eligible
model wins (`preferred fable not eligible; downgraded to opus`). When nothing
in the list is eligible and it was not a hard override, the first eligible
candidate in provider order wins (`nothing in the preference list is
eligible; falling back to gpt-6-luna (first eligible on codex in provider
order)`); without any preference (critical/high) that is simply the most
capable eligible model of the first provider that has one. A hard override
never crosses to another provider and is never upgraded: if its chain is out,
the task waits.

**Per-model verdict.** Every candidate is checked (`Policy::verdict`); the
first failing gate gives the reason:

1. **Observed cooldown.** The provider's latest observation says blocked or
   fully used (see above).
2. **Rate limit.** A model on cooldown is out until it ends
   (`rate limited until <time>`, with `(claude account-wide)` when every
   model of the provider is cooling down).
3. **Criticality gate.** The task must be at least `min_criticality`, or the
   tier must be *relaxed*:
   - after `relax_after_fraction` of the period, if the tier's spend fraction
     (`period_weighted / period_budget`) is below the period's elapsed
     fraction (the tier is under-paced), one criticality level lower
     qualifies;
   - in the **end game** (elapsed ≥ `endgame_fraction`, default 0.8), two
     levels lower qualify regardless of pacing.
   Reason when blocked: `reserved for critical (relaxed to high); task is low
   (20% of its budget spent at 68% of the period)`.
4. **Tier share, for borrowed use only.** When the task is less critical
   than the model's `min_criticality` (it got in through relaxation),
   `predicted × weight` must fit the tier's remaining share times
   `1 - safety_margin` (`opus's share: needs 3868619 weighted tokens,
   1964258 left after the safety margin`). Work the model is reserved for
   skips this step.
5. **Period allowance.** The provider's period fraction (from its own
   reading when there is one, else measured) plus this task's cost at the
   effective budget must stay below `1 - safety_margin` (`period allowance:
   43% used (observed), this task would push it to 46%`).
6. **Window** (when the provider has one, configured or reported). The
   window fraction plus the cost at the effective window budget must stay
   below `1 - safety_margin` (`window: 91% used (observed), this task would
   push it to 97%`); the retry waits for the reported window reset when it
   comes sooner than the 15-minute recheck.

An eligible tier reads `eligible; 20% of its budget spent at 68% of the
period` (or `eligible (relaxed to high); ...`).

**Throttle.** If nothing fits the task gets no model and a `retry_at`: the
earliest hint across providers among the blocking models (rate-limit or
observed cooldown end, the next relaxation or end-game boundary, a period
end, or `WINDOW_RECHECK` = 15 minutes when the window was the blocker),
capped at the earliest period end and never in the past. For a hard override
only its own provider's hints count.
The task state becomes `throttled` and `not_before` is set; `pick_next` takes
it again once that passes.

Several launches in one tick reserve their predicted cost in the in-memory
ledger so they do not all see the same headroom. The whole trail is stored
in the `task.starting` / `task.throttled` event and printed by
`powerqueue task explain`.

## Rate-limit cooldowns

Claude Code does not exit on a rate limit; it fires a `StopFailure` hook with
`error_type: rate_limit` (also `overloaded`, `usage_limit`, `quota`). The
daemon looks at the provider of the session's model and cools it down for
that provider's `rate_limit_cooldown_mins` (default 30; at least one minute,
never past that provider's period end): subscription limits are account-wide,
so a `rate_limit` marks every model of the provider; `overloaded` marks only
the model that reported it. It logs `budget.rate_limited` (naming the
provider) and puts the task in `throttled` with `not_before` at the cooldown
end. Meanwhile other providers keep running tasks. If the session is still
alive when the cooldown passes the task simply goes back to `running`;
otherwise it is relaunched like any throttled task. The state is persisted in
the `kv` table under `budget.rate_limits` (`RATE_LIMITS_KEY`) so a daemon
restart does not forget it. `powerqueue budget clear-limits [--provider P]`
forgets it on purpose, for example after `/usage` shows the window has reset.

Recent Claude Code versions wait in the session instead and print `Usage
limit reached · continuing automatically at 3:45pm`. A pane showing that
text is *waiting*, not hung: while the provider is on cooldown the session is
neither marked stale (`scheduler.stale_session_secs`) nor killed for
`max_session_secs`; the task shows as `throttled` until the cooldown ends
(event `session.waiting_for_reset`). If no hook reported the limit, the pane
text itself starts the provider's cooldown.

## Learning the rate

`period_weighted_tokens` says how many weighted tokens one whole allowance
is worth. Nobody knows that number: the weights are an approximation of how
the provider meters usage, and interactive work in other terminals drains
the same allowance. So the ledger learns it (`ledger::learn_rate`,
`LearnedRate` in `budget show --json`):

- Take the readings of the current period (kv `budget.observations.<p>`;
  a pre-0.7 `budget set-observed` calibration counts as one reading).
- Every pair of readings is a candidate when the percentage grew by at
  least `MIN_OBSERVED_DELTA` = 2 points between them, the two never pair
  across a decrease (a reset), and at least `MIN_MEASURED_DELTA` = 1M
  tier-weighted tokens were recorded between the two instants (less means
  the jump came from usage powerqueue cannot see). The spend between two
  instants comes from a per-minute series of the usage table
  (`Store::usage_by_minute`), so hundreds of readings cost one query.
- Each candidate gives `Δmeasured / Δobserved`; the **largest** wins and
  becomes the effective `period_budget`. Usage outside powerqueue (another
  terminal) only ever moves the percentage *more* than we measured, so
  every candidate is a lower bound on the true allowance and the largest is
  the least contaminated. The tier shares, the pacing comparisons and the
  policy's cost checks all use it; without a candidate the configured
  budget stays in force.

The window rate is learned the same way from pairs at most
`WINDOW_LEARN_SPAN` = 1 hour apart, so what rolled out of the window in
between stays small; without one, `window_weighted_tokens` applies, and
with neither the window is judged on the reading alone. A window reading
is trusted until its reported reset (or for five hours when none is
known), so a reading from a window that has rolled over cannot keep
blocking launches after the last session ended.

Claude's status line delivers a reading after every response, so the rate
is usually known within the first hour of a period and refines itself as
the mix of cache reads and output changes. `budget show` prints the rate
(`100% of the period ≈ 540.0M weighted tokens (configured 80.0M): 2.1
points moved for 11.3M tokens between …`), `doctor` warns when the
configured budget is more than 2× off and tells you the value to put in
`period_weighted_tokens` so pacing is right before a period's first readings.

Two commands feed outside knowledge in by hand:

- `budget set-reset <when>` stores `period_anchor` so period boundaries are
  right (unnecessary once a probe has reported the reset).
- `budget set-observed 43%` records what `/usage` shows as a reading, for
  providers without a probe or before a session has answered.

The pre-0.7 additive calibration (kv `budget.calibration.<p>`) is no longer
read.

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
- Decision: **Fable**. Reasons: `predicted cost 600000 weighted tokens
  (label=incident (n=3))`, `claude: period 5% elapsed, 0% spent; window 0%
  used`, `fable: eligible; 0% of its budget spent at 5% of the period`, ...,
  `preferred by rules: fable`, `preferred fable is eligible`.

With Codex enabled too and `## Models` saying `critical: fable |
gpt-6.1-sol`, a Claude rate limit (`fable: rate limited until … (claude
account-wide)`) would send the same ticket to `gpt-6.1-sol` (`preferred fable
not eligible and no cheaper tier is`, `preferred gpt-6.1-sol is eligible`).

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
`budget.providers.claude.models.fable.min_criticality = "high"` or add a scoring rule that
raises the chore to `normal` on Fridays (edit PRIORITY.md; it reloads live).

## Tuning with `doctor`

`powerqueue doctor` has an *algorithm* category (the anchor check sits under
*configuration*). Each finding comes with a concrete suggestion
(`src/doctor.rs`):

| Check | Warns when | Hint |
|-------|------------|------|
| `<provider>` budget anchor | `period_anchor` not set for an enabled provider | `budget set-reset <time from /usage> --provider <p>` |
| config.toml | old flat `[budget]` keys found | move them under `[budget.providers.claude]` (`config set` does it) |
| `codex` / `gemini` | enabled but binary missing, not logged in, or (always) experimental | install / log in, or keep the provider disabled |
| cost estimator | fewer than 5 samples, or leave-one-out error above 60% | let tasks finish; add Linear estimates and consistent labels so buckets form |
| fable reservation | under-used: > 70% of the period gone with < 30% of Fable's share spent | lower `models.fable.relax_after_fraction` or `min_criticality` |
| fable reservation | over-paced: spent fraction more than 25 points ahead of elapsed | raise `models.fable.min_criticality` or lower its share |
| window pressure | more than 90% of the window used (observed when the provider reports it) | lower `scheduler.max_concurrent` or wait for the roll-over |
| `<provider>` usage rate | the learned rate says the configured `period_weighted_tokens` is more than 2× off | set it to the learned value (printed) |
| fable share | Fable's share is smaller than one typical task × its weight: it could never lend itself to less critical work | raise `models.fable.share` or lower `models.fable.weight` |
| scheduling (*state*) | `powerqueue pause` is in effect | `powerqueue resume` |
| throttling | more than 10 `task.throttled` events in 24 h | raise `period_weighted_tokens` / `window_weighted_tokens` or lower expensive shares |
| crash rate | `session.crashed` over `session.launched` above 30% in 7 days | check `claude.permission_mode` and `repo.setup`; raise `stale_session_secs` if work is just slow |
| idle / attention rate | `session.nudged` + `task.needs_attention` + `task.blocked` + `session.permission_prompt` events over launches above 30% | tighten the completion protocol (always run `task complete`) or use a less interactive permission mode |

`budget show` tells you whether the rate is learned for the current period
and from how many readings. `doctor --json` includes the detail text of
each finding so you can track it.
