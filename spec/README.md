# Quint: queue, launch, crash, budget, execution projects, admission, routing

This specification models queue lifecycle and launch scheduling.
It supplements the Rust properties and whole-daemon model. It does not replace
those tests or prove the implementation equivalent to the specification.

## Run

The replay adapter uses the project's Rust toolchain and dependencies. Node 22
and npm run Quint; Python 3 is needed only by the fake-agent e2e fixtures,
not by the adapter or its tests:

```sh
npm ci --prefix spec --ignore-scripts
npm --prefix spec run typecheck
npm --prefix spec test
spec/node_modules/.bin/quint test spec/powerqueue.qnt --main=progress
npm --prefix spec run check
cargo test --example quint-replay -- --include-ignored
```

Quint is locked to 0.33.0. Its `adm-zip` dependency is overridden to 0.6.1
(the fixed version); `npm audit` is clean. Quint downloads its Rust evaluator
on first use. `check` runs 1,000 samples of up to 100 steps with seed 42.
Both the scheduler and assignment-race modules run in `test` and `check`,
including advisory CI. Simulation is randomized testing, **not exhaustive verification**.

For bounded symbolic checking, install Java and run:

```sh
spec/node_modules/.bin/quint verify spec/powerqueue.qnt --invariant=safety --max-steps=5
spec/node_modules/.bin/quint verify spec/powerqueue.qnt --main=progress --backend=tlc --temporal=eventuallyStarts
npm --prefix spec run verify:assignment
```

Quint downloads Apalache/TLC on first use. The finite progress model uses TLC
for full temporal checking; TLC also exhaustively checks the finite assignment-race
model (34 reachable states). Apalache checks safety to the requested depth. These commands are deliberately outside
the normal Rust gate and advisory CI simulation job. See the
[Quint CLI documentation](https://quint.sh/docs/quint) for backend requirements.

## Model and assumptions

`lifecycle` defines the shared transition guards and updates. `powerqueue`
uses them in a three-task, two-provider, three-project scheduler with two slots, unit-cost
launches, and a one-unit budget per provider. A pass considers each task once.
Successful launches reserve cost; a failed start consumes neither a slot nor
budget. `spent` is a **pass-local reservation**, reset on the next pass, not
historical token usage. Provider cooldowns expire with time. The model checks
slot and budget bounds, serialized launches, retry-time state restrictions,
the crash-attempt limit, and a history witness that every start/launch had approval. A task has at most one live session by its type;
the replay additionally rejects a launch while that task already has one.

GitHub admission is an explicit lifecycle value. `Approve` models verified approval;
`Revoke` abstracts label removal, material edits, untrusted actors, or unavailable
evidence. It can interleave with setup and active sessions. Both `Start` and
`Launch` require approval; `Hold` returns a revoked setup to the queue without
consuming a slot/reservation or removing artifacts. Resume/retry preserve the
admission value. Active sessions may finish after revocation: the invariant is
that **new starts are authorized**, not that every existing session remains approved.
The approval evaluator and GitHub pagination/actor/edit parsing are exercised by
Rust integration tests; Quint assumes their resulting decision, rather than proving
the external API or trust policy. Remote changes after the final verification response
are outside the atomic `Launch` abstraction.

The Rust one-task and whole-daemon models generate approved, denied and unverified
GitHub tasks, admission changes, restart round trips and revocation during setup.
A separate SQLite stateful property interleaves saved candidates, approval changes,
material edits, re-reads after restart, and conditional claims. It checks that stale
approval/scope cannot claim a start. These models call production transitions;
Quint supplies the independent abstract lifecycle guards. A SQLite property also
checks that a bound GitHub task’s new attempt, branch and worktree claim survive
the post-setup approval reload, including denied evidence. The atomic start now
persists the claim; Quint already makes Start update attempts and artifacts together.

Source-to-project reconciliation is also an operation in both Rust stateful
models. Generated source destinations and approval decisions interleave with
reassignment, retry, restart, review and launch. A forced lifecycle sequence
checks that a mapping conflict holds setup, survives retry/restart, preserves
active work and review history, and permits a later launch after the original
mapping is restored. Launch passes also exercise mapping changes during setup,
checking that refused launches consume neither a slot nor a budget reservation
and do not alter other tasks. These operations exercise the production routing
admission transition. Registry lookup, source credentials, multi-repository
identity, HTTP failures and preview/apply races remain integration-test concerns;
Quint models routing eligibility independently from source approval rather than
modeling the source registry or network. Both are required at Start and Launch;
restoring one cannot bypass a hold from the other. RouteAllow/RouteBlock can
interleave with setup and active work, and Hold preserves prepared artifacts.
Retry/resume do not restore either prerequisite.

`Start` combines `on_starting` with `claim`: the observed attempt is reserved
at that point. The daemon finishes a start within a tick. Commands in this
cut run between passes, except project reassignment attempts, which can also
interleave with a launch and must then be rejected. Paused and throttled tasks may retain live sessions.
A direct CLI completion changes the task before the daemon closes its session;
`Complete` and `Release` represent those separate steps. A done marker can
instead close that still-live session with `Finish`.

Execution projects carry separate configured checkout identities, immutable task
bindings, and artifact ownership. Configuration can remove/restore a project or
move/restore its checkout. Missing projects are excluded; mismatched pinned
checkouts are rejected before repository operations, using no slot or reservation.
The `projectSafety` invariant checks task assignment, binding, artifact ownership,
and the target of the last abstract repository operation. A write-once history
witness checks that later starts cannot overwrite a task's original binding.
Provider allocation stays
independent of project assignment, so different projects contend for the same budget.
An abstract repository operation represents ownership checks for push/cleanup;
filesystem canonicalization and git common-directory checks remain integration tests.

`assignment_race` separately models a daemon's saved candidate, CLI reassignment,
conditional launch claim, binding, completion, and retry. This exposes interleavings
hidden by the scheduler's atomic `Start`: a stale project cannot claim the task,
and a claimed task cannot be reassigned, even before its binding is persisted.
After completion/retry, a retained binding still prohibits reassignment. This finite
model abstracts state/project guards; the Rust SQLite property additionally checks
actual row updates and isolation between two task rows. It does not model every
state/attempt guard or prove SQLite equivalent to the specification.

The model reflects two details of the Rust lifecycle:

- A terminal task can temporarily retain a session after a direct completion
  or source cancellation; cleanup releases it later.
- `max_attempts` is a crash cutoff, not an unconditional lifetime bound. Manual
  retries of nonterminal throttled tasks preserve attempts. Retrying a terminal
  task resets them. Review rounds, which also affect attempts, are out of scope.

The `progress` module isolates the liveness claim: with persistent approval, budget and
a free slot, no competing tasks, and weak fairness for time and launch, a queued
or throttled task eventually starts. Its finite clock saturates after the retry
time. This is a conditional property, not a starvation-freedom claim about the
real priority scheduler. There is no per-task progress guarantee under arbitrary
pause/cancel commands, competing higher-priority work, or perpetual budget exhaustion.

Not modeled: PR review, relayed answers, dependency changes, configuration changes
beyond project availability/checkout identity, provider/model selection, observed usage, floating-point costing, or actual
rolling spend. Rust properties still cover these arithmetic/policy details.

## Replay real event logs

Export JSON with `powerqueue logs --events --json -n 100000 > events.json`.
Use a complete history beginning with task creation; filtered or partial histories
are reported as skipped. UUIDs become local integer task indices. Ticket titles,
paths, hook bodies and other raw text are not written to generated traces.

```sh
cargo run --quiet --example quint-replay -- events.json --name queue --run
```

The Rust adapter (`spec/replay.rs`, a Cargo example) reads the application's
`Event` type, sorts by store event ID, converts times to integer seconds from the
earliest timestamp, and generates a deterministic `replayTest` under `.generated/`.
Every observation asserts the **same lifecycle guard used by the simulation**,
then checks logged attempt numbers. An illegal step fails an assertion, rather
than silently stopping a simulation at a disabled action. Quint emits ITF state
traces, including counterexamples, alongside a coverage report. A trace without
any in-scope launch is rejected as vacuous. Successful replay means the observed
sequence is admitted by this abstraction, not that every database row matched it.

Replay cannot reconstruct pass boundaries, predicted-cost reservations, complete
budget ledgers, or actual simultaneous session counts from these logs. It does
not validate those invariants. Replay still projects only lifecycle state:
`task.project_set` and `task.project_bound` stutter, so replay does not validate
project isolation. Project safety is checked by the simulations/models above and
Rust properties/integration tests, not by the event-log projection. Task idle/running changes are collapsed into an
active state because some progress changes emit no event. Crash delay is available
only in the v0.14 message (`retrying in Ns`); the adapter reads it strictly, checks
that the next start respects it, but cannot verify it against historical config.
Timestamps are coarse and record logging time rather than transition time.

Replay distinguishes GitHub creation (initially unverified) from ordinary creation
(initially admitted). It consumes `github.admitted`, `github.revoked`,
`github.admission_blocked`, `routing.selected`, `routing.blocked`, and both
`github.launch_held` and `routing.launch_held`; unknown GitHub/routing events must be
classified explicitly. Denial/reapproval schedules are tested both positively and
negatively, including attempts to bypass approval through pause/resume/retry and
launches after revocation during setup. Replay verifies the **logged** approval
and routing decisions and launch ordering, not the underlying GitHub evidence or
routing predicates. Older traces without routing events retain the legacy default
of permitted routing. Source approval and routing are independent in every trace.

At the first unsupported lifecycle event for a task (review, blocker, relay,
dependency, launch error/supersession), replay **ends that task's prefix**. It
never skips a state change and then resumes checking from stale state. The report
names every cutoff. Known observational events stutter; unknown task/session
kinds fail until explicitly classified. A `review.lookup_failed` is observational:
it leaves the completed task unchanged. Linear sync's daemon echoes of creation
and cancellation are recognized separately from the original transition.

| Event kind | Action |
| --- | --- |
| `task.created` | Create / CreateGitHub (unverified) |
| `github.admitted`, `github.revoked`, `github.admission_blocked` | Approve / Revoke |
| `routing.selected`, `routing.blocked` | RouteAllow / RouteBlock (independent of approval) |
| `github.launch_held`, `routing.launch_held` | Hold (preserves setup artifacts) |
| `task.starting`, `session.launched` | Start, Launch |
| `session.crashed`, `task.failed` | Crash, Fail (check logged attempt and limit) |
| `task.completed`, `task.completed_by_command` | Finish, Complete |
| `session.finalized` | Release |
| `task.throttled` | Throttle |
| `budget.rate_limited` from a hook, `session.waiting_for_reset` | RateLimit (retains session) |
| `budget.rate_limited` with `source=pane` | Stutter: provider mark alone does not change task |
| `task.resumed` after cooldown | Wake |
| `task.paused`, ordinary `task.resumed`, `task.cancelled`, `task.retried` | Pause, Resume, Cancel/SourceCancel, Retry |

To capture the four supported real-daemon scenarios without adding Quint to
`cargo test`:

```sh
POWERQUEUE_TRACE_DIR=/tmp/pq-traces cargo test --test e2e_daemon
for name in completion crash-resume rate-limit github-approved; do
  cargo run --quiet --example quint-replay -- "/tmp/pq-traces/$name.json" --name "$name" --run
done
```

The advisory workflow requires all four artifacts, so skipped e2e tests cannot
produce a false success. Adapter tests include negative schedules: double launch,
wrong attempt, premature restart, early failure, crash past the limit, and duplicate
completion after a session has ended. Projection tests run with ordinary
`cargo test`; tests that invoke Quint are explicitly ignored there and required
by the advisory command above. A missing Quint executable fails that command.
The adapter requires the full application event envelope (including UUID task
IDs and event level), timezone-bearing RFC 3339 timestamps, and positive integer
attempts and limits. It retains microsecond truncation before converting to
coarse seconds; timestamp fields in coverage reports use normalized UTC.
Crash replay uses the recorded `retry_at` deadline, since event persistence can
lag the transition's clock. Older events without that field fall back to the
logged timestamp plus the message's backoff duration; logging delays can make
that approximation reject an otherwise valid restart. CI retains generated
replay schedules alongside coverage reports and counterexamples for diagnosis.

## Operational validation

Replay logs from a consistent application version and retain task creation
histories within the selected window. Tasks whose creation falls outside that
window are skipped; inspect the coverage report before interpreting a pass.
Historical logs may contain behavior that the current implementation no longer
allows. Investigate rejected transitions against the version that emitted them
rather than weakening the model to accept them.

Use synthetic regression fixtures for invalid schedules, such as a second
completion after a session has ended. Keep raw operational logs and generated
traces outside version control.
