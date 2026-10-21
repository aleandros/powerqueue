# Kani: bounded budget proofs

These optional harnesses call production functions directly, under
`#[cfg(kani)] mod verification` in the budget modules. They compile out of
normal builds. The [Quint specification](README.md) is the complementary
scheduler model.

## Run

Install [Kani 0.68.0](https://model-checking.github.io/kani/install-guide.html)
(the version pinned by CI):

```sh
cargo install --locked kani-verifier --version 0.68.0
cargo kani setup
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target-kani cargo kani --lib -Z unstable-options --harness-timeout 300s --output-format terse --jobs 2
# One proof:
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target-kani cargo kani --lib -Z unstable-options --harness-timeout 300s --harness kani_periods_contain_now_and_tile
```

Kani installs its own pinned nightly. Use a separate build directory from the
ordinary Rust gate, and never share it across checkouts. `cfg(kani)` is registered
in Cargo's lint configuration; ordinary unexpected cfg names still warn. The
timeout option requires Kani's `unstable-options` flag. A timeout or insufficient
unwind bound is a failure, not a proof. No unwind assertions are disabled.
Incremental compilation is disabled: switching from filtered to all-harness
verification with reused incremental artifacts triggered a Kani 0.68.0 compiler
panic in `kani_middle/analysis.rs`; recompiling without incremental artifacts
allowed verification to proceed. A compiler panic never counts as a proof.

`.github/workflows/kani.yml` runs on relevant PRs and manual dispatch. Its job
is advisory (`continue-on-error: true`), with a 30-minute job limit and 300 seconds
per harness. It does not change release behavior or add a required Rust check.
The `kani-results` artifact contains Kani's JSON report, including individual
harness outcomes, check counts and timing metadata. It is uploaded even after a
verification failure; a missing report (for example, compilation failure) is not
evidence of success. Inspect the job log as well as the report.

## What is bounded

| Harness | Domain and claim |
| --- | --- |
| `kani_periods_contain_now_and_tile` | Period lengths 1–120 seconds; whole-second offsets -360–360 around Unix epoch. Current period contains now, adjacent periods touch exactly, and the 60-second rolling window ends at now. |
| `kani_period_fraction_and_remaining` | Ten-second period, offsets -2–12 seconds. Fractions stay in [0,1] and clamp at either end; remaining time is exact; a zero-length window is empty. |
| `kani_rate_limit_mark_boundary` | One Claude tier, deadlines and current times 0–4 seconds (25 combinations). Exhaustion and the returned deadline match strict timestamp ordering; a mark is usable again exactly at expiry. |
| `kani_add_spend_conserves_totals` | One configured tier, three successive costs: first from {0, 0.25, 4}, second from {0.25, 4}, third from {0, 1}. Tier, period, window and observation counters agree (12 combinations). |
| `kani_add_spend_isolates_two_tiers` | Sonnet and Haiku start with period/window spend 1 and 2, provider totals 3 and spend since observation 0.5. Costs use the same three domains as above; the first charges Sonnet, the second Haiku, the third either tier (24 combinations). Each tier receives exactly its own charges; shared period/window totals and the observation counter increase by the sum. |
| `kani_thin_samples_preserves_order_age_and_spacing` | Two ordered samples, newest age in {0, 7200, 777600} seconds and gap in {0, 60, 600}. Output length is bounded, age and spacing restrictions hold, and the newest eligible timestamp is retained. |

These assumptions bound the claims; they do not cover all f64 values, all chrono
dates, arbitrary model names, complete configs or the full decision procedure.
Inputs avoid chrono's extreme-duration panics. Ledger conservation assumes the
tier exists, as it does for a configured launch candidate. Sample ordering is a
precondition of `thin_samples`, not something it sorts. Heap-backed harness data
is forgotten after assertions to keep unrelated collection destruction outside
the proof; the production functions and memory checks within them are unchanged.

The sample harness uses unwind 8 even with only two readings: Rust's internal
byte-swap loop also needs a sufficient bound. Unwind assertions remain enabled.

## Larger-domain evaluation (#50)

Measured on 2026-10-21 with Kani 0.68.0, its Rust 1.100.0 nightly
(`8925ea358`, 2026-08-20), CBMC 6.11.0 and the default CaDiCaL solver on
`aarch64-apple-darwin`. These are single local measurements, not performance
guarantees; other compilation/test work was running during the cooldown runs.
All runs used unwind 8, default memory/overflow checks, assertion reachability
checks and a 300-second per-harness limit. No contracts or stubs were used.

| Domain | Symbolic execution | Expression steps | SAT solver time | Verification time / outcome |
| --- | ---: | ---: | ---: | --- |
| Existing single-mark boundary | 4.844 s | 81,836 | 11.275 s | 20.121 s, success |
| Two-tier spend conservation and isolation | 1.365 s | 69,025 | 2.031 s | 6.494 s, success |
| Single-tier deadline overwrite | 93.883 s | 1,349,685 | Not reached in reported stages | 300 s limit, timeout (no proof) |

Verification time excludes compilation. Solver time is the **sum** of all
`Runtime Solver` lines in the verbose log; the JSON `runtime_solver_s` field
reports only the last solver iteration in this version. Symbolic execution and
solver time do not include every verification stage, so they need not add up to
the total. Reproduce a retained domain and capture both kinds of evidence with:

```sh
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target-kani cargo kani --lib -Z unstable-options \
  --harness-timeout 300s --harness kani_add_spend_isolates_two_tiers \
  --verbose --export-json /tmp/kani-two-tiers.json > /tmp/kani-two-tiers.log 2>&1
```

The retained extension calls production `Ledger::blank` and `Ledger::add_spend`.
Two explicitly configured, distinct Claude tiers keep this independent from
provider routing, policy eligibility and map mutation. Initial nonzero balances
check accumulation; a different observation counter checks that it is incremented
rather than replaced by the provider total. All chosen sums are exactly
representable binary fractions. This proves neither arbitrary floating-point
arithmetic nor two-provider `reserve`. The original five harnesses are retained.

The timed-out overwrite attempt called production `RateLimitState::mark` twice
for Sonnet. Original deadline, replacement deadline and current time were
independent `u8` values constrained to 0–4 seconds after Unix epoch (125
combinations). Assertions required one map entry containing the replacement,
`is_exhausted == (now < replacement)`, and `until` equal to the replacement only
while it was in the future. This included both shortening and extending a mark.
The second insertion expanded Rust's B-tree paths before solving: postprocessing
reported 78,196 verification conditions, 28,948 remaining after simplification,
but no SSA-conversion or solver-completion timing before timeout. Removing
expiry clearing from the combined overwrite/clear experiment did not make this
domain fit the limit. Neither attempt is retained in the default harness set;
neither establishes cooldown clearing or overwrite correctness. No production
defect was found, so no runtime change or regression fix accompanies this work.

## Deferred scope

Two-provider `reserve`, cooldown clearing/overwrites, arbitrary tier sets,
`provider_blocked_until`, full `Policy::eligibility`, and larger history samples
remain outside the retained proof domains. Only the two-tier arithmetic domain
above was added; the other candidates were not evaluated in this pass beyond
the documented cooldown attempts. Symbolic execution and heap-pointer
simplification can make these larger domains expensive before SAT solving begins.
Extending coverage may require library contracts or further decomposition; no
successful proof is claimed for these cases.

The existing proptest suite still covers provider routing, complete policy
eligibility and broad observation histories. Kani adds exhaustive checks within
the small domains above; it does not replace those tests. Inspect individual
harness results: a nonblocking CI job's overall status alone is not proof evidence.
