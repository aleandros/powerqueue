# Quint: queue, launch, crash, budget

This is the first specification cut from `HANDOFF-formal-testing.md`.
It supplements the Rust properties and whole-daemon model. It does not replace
those tests or prove the implementation equivalent to the specification.

## Run

Node 22, Python 3.9+, and npm are sufficient for the advisory gate:

```sh
npm ci --prefix spec --ignore-scripts
npm --prefix spec run typecheck
npm --prefix spec test
spec/node_modules/.bin/quint test spec/powerqueue.qnt --main=progress
npm --prefix spec run check
python3 -m unittest discover -s spec -p 'test_*.py'
```

Quint is locked to 0.33.0. Its `adm-zip` dependency is overridden to 0.6.1
(the fixed version); `npm audit` is clean. Quint downloads its Rust evaluator
on first use. `check` runs 1,000 samples of up to 100 steps with seed 42.
Simulation is randomized testing, **not exhaustive verification**.

For bounded symbolic checking, install Java and run:

```sh
spec/node_modules/.bin/quint verify spec/powerqueue.qnt --invariant=safety --max-steps=5
spec/node_modules/.bin/quint verify spec/powerqueue.qnt --main=progress --backend=tlc --temporal=eventuallyStarts
```

Quint downloads Apalache/TLC on first use. The finite progress model uses TLC
for full temporal checking; Apalache checks safety to the requested depth. These commands are deliberately outside
the normal Rust gate and advisory CI simulation job. See the
[Quint CLI documentation](https://quint.sh/docs/quint) for backend requirements.

## Model and assumptions

`lifecycle` defines the shared transition guards and updates. `powerqueue`
uses them in a three-task, two-provider scheduler with two slots, unit-cost
launches, and a one-unit budget per provider. A pass considers each task once.
Successful launches reserve cost; a failed start consumes neither a slot nor
budget. `spent` is a **pass-local reservation**, reset on the next pass, not
historical token usage. Provider cooldowns expire with time. The model checks
slot and budget bounds, serialized launches, retry-time state restrictions,
and the crash-attempt limit. A task has at most one live session by its type;
the replay additionally rejects a launch while that task already has one.

`Start` combines `on_starting` with `claim`: the observed attempt is reserved
at that point. The daemon finishes a start within a tick. Commands in this
cut run between passes. Paused and throttled tasks may retain live sessions.
A direct CLI completion changes the task before the daemon closes its session;
`Complete` and `Release` represent those separate steps. A done marker can
instead close that still-live session with `Finish`.

Two proposed handoff invariants required qualification against actual Rust:

- A terminal task can temporarily retain a session after a direct completion
  or source cancellation; cleanup releases it later.
- `max_attempts` is a crash cutoff, not an unconditional lifetime bound. Manual
  retries of nonterminal throttled tasks preserve attempts. Retrying a terminal
  task resets them. Review rounds, which also affect attempts, are out of scope.

The `progress` module isolates the liveness claim: with persistent budget and
a free slot, no competing tasks, and weak fairness for time and launch, a queued
or throttled task eventually starts. Its finite clock saturates after the retry
time. This is a conditional property, not a starvation-freedom claim about the
real priority scheduler. There is no per-task progress guarantee under arbitrary
pause/cancel commands, competing higher-priority work, or perpetual budget exhaustion.

Not modeled: PR review, relayed answers, dependency changes, config reloads,
provider/model selection, observed usage, floating-point costing, or actual
rolling spend. Rust properties still cover these arithmetic/policy details.

## Replay real event logs

Export JSON with `powerqueue logs --events --json -n 100000 > events.json`.
Use a complete history beginning with task creation; filtered or partial histories
are reported as skipped. UUIDs become local integer task indices. Ticket titles,
paths, hook bodies and other raw text are not written to generated traces.

```sh
python3 spec/replay.py events.json --name queue --run
```

The adapter sorts by store event ID, converts times to integer seconds from the
first event, and generates a deterministic `replayTest` under `.generated/`.
Every observation asserts the **same lifecycle guard used by the simulation**,
then checks logged attempt numbers. An illegal step fails an assertion, rather
than silently stopping a simulation at a disabled action. Quint emits ITF state
traces, including counterexamples, alongside a coverage report. A trace without
any in-scope launch is rejected as vacuous. Successful replay means the observed
sequence is admitted by this abstraction, not that every database row matched it.

Replay cannot reconstruct pass boundaries, predicted-cost reservations, complete
budget ledgers, or actual simultaneous session counts from these logs. It does
not validate those invariants. Task idle/running changes are collapsed into an
active state because some progress changes emit no event. Crash delay is available
only in the v0.14 message (`retrying in Ns`); the adapter reads it strictly, checks
that the next start respects it, but cannot verify it against historical config.
Timestamps are coarse and record logging time rather than transition time.

At the first unsupported lifecycle event for a task (review, blocker, relay,
dependency, launch error/supersession), replay **ends that task's prefix**. It
never skips a state change and then resumes checking from stale state. The report
names every cutoff. Known observational events stutter; unknown task/session
kinds fail until explicitly classified. A `review.lookup_failed` is observational:
it leaves the completed task unchanged. Linear sync's daemon echoes of creation
and cancellation are recognized separately from the original transition.

| Event kind | Action |
| --- | --- |
| `task.created` | Create |
| `task.starting`, `session.launched` | Start, Launch |
| `session.crashed`, `task.failed` | Crash, Fail (check logged attempt and limit) |
| `task.completed`, `task.completed_by_command` | Finish, Complete |
| `session.finalized` | Release |
| `task.throttled` | Throttle |
| `budget.rate_limited` from a hook, `session.waiting_for_reset` | RateLimit (retains session) |
| `budget.rate_limited` with `source=pane` | Stutter: provider mark alone does not change task |
| `task.resumed` after cooldown | Wake |
| `task.paused`, ordinary `task.resumed`, `task.cancelled`, `task.retried` | Pause, Resume, Cancel/SourceCancel, Retry |

To capture the three supported real-daemon scenarios without adding Quint to
`cargo test`:

```sh
POWERQUEUE_TRACE_DIR=/tmp/pq-traces cargo test --test e2e_daemon
for name in completion crash-resume rate-limit; do
  python3 spec/replay.py "/tmp/pq-traces/$name.json" --name "$name" --run
done
```

The advisory workflow requires all three artifacts, so skipped e2e tests cannot
produce a false success. Adapter tests include negative schedules: double launch,
wrong attempt, premature restart, early failure, crash past the limit, and duplicate
completion after a session has ended.

## Validation (2026-10-10)

Local checks passed: Rust fmt/clippy/full tests (590 unit tests and all integration
suites), three scheduler scenarios plus two progress scenarios, 1,000 simulations
of 100 steps, 15 adapter/negative replay tests, and all three real e2e traces.
Apalache checked `safety` through five transitions; TLC exhaustively checked the
finite `progress` model's `eventuallyStarts` temporal property. Removing fairness
from that property produces a TLC counterexample (the capped clock can stutter
forever before launch). Neither result
extends beyond the abstractions and assumptions above.

## Real queue evidence (2026-10-10)

Read-only export from `ssh dev`: 67,504 events spanning October 5–10. The running
binary is 0.14.0, activated at 19:50:07 UTC October 10. The export ends with its
startup; **there is no post-upgrade task activity yet**, so this is historical
evidence, not the requested full day on 0.14.0.

The historical replay rejects event 3558: a second `task.completed` after event
3556 already completed the task and ended its session. That behavior predates
0.14.0; the current dead-session guard logs `hook.duplicate` instead. A synthetic
negative test preserves the check without committing production data. Do not
weaken the guard to make an old trace pass. Re-export after a day of actual
queue activity, retaining creation histories, and inspect the coverage report.
