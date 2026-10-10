# Kani: bounded budget proofs

These optional harnesses call production functions directly, under
`#[cfg(kani)] mod verification` in the budget modules. They compile out of
normal builds. This is the first Kani cut from `HANDOFF-formal-testing.md`;
the [Quint specification](README.md) is the complementary scheduler model.

## Run

Install [Kani 0.68.0](https://model-checking.github.io/kani/install-guide.html)
(the version pinned by CI):

```sh
cargo install --locked kani-verifier --version 0.68.0
cargo kani setup
CARGO_TARGET_DIR=target-kani cargo kani --lib -Z unstable-options --harness-timeout 120s --output-format terse --jobs 2
# One proof:
CARGO_TARGET_DIR=target-kani cargo kani --lib --harness kani_periods_contain_now_and_tile
```

Kani installs its own pinned nightly. Use a separate build directory from the
ordinary Rust gate, and never share it across checkouts. `cfg(kani)` is registered
in Cargo's lint configuration; ordinary unexpected cfg names still warn. The
timeout option requires Kani's `unstable-options` flag. A timeout or insufficient
unwind bound is a failure, not a proof. No unwind assertions are disabled.

`.github/workflows/kani.yml` runs on relevant PRs and manual dispatch. Its job
is advisory (`continue-on-error: true`), with a 30-minute job limit and 120 seconds
per harness. It does not change release behavior or add a required Rust check.

## What is bounded

| Harness | Domain and claim |
| --- | --- |
| `kani_periods_contain_now_and_tile` | Period lengths 1–120 seconds; whole-second offsets -360–360 around Unix epoch. Current period contains now, adjacent periods touch exactly, and the 60-second rolling window ends at now. |
| `kani_period_fraction_and_remaining` | Ten-second period, offsets -2–12 seconds. Fractions stay in [0,1] and clamp at either end; remaining time is exact; a zero-length window is empty. |
| `kani_rate_limit_mark_boundary` | One Claude tier, deadlines and current times 0–4 seconds (25 combinations). Exhaustion and the returned deadline match strict timestamp ordering; a mark is usable again exactly at expiry. |
| `kani_add_spend_conserves_totals` | One configured tier, two successive costs from {0, 0.25, 4}. Tier, period, window and observation counters agree. |
| `kani_thin_samples_preserves_order_age_and_spacing` | Two ordered samples, newest age in {0, 7200, 777600} seconds and gap in {0, 60, 600}. Output length is bounded, age and spacing restrictions hold, and the newest eligible timestamp is retained. |

These assumptions bound the claims; they do not cover all f64 values, all chrono
dates, arbitrary model names, complete configs or the full decision procedure.
Inputs avoid chrono's extreme-duration panics. Ledger conservation assumes the
tier exists, as it does for a configured launch candidate. Sample ordering is a
precondition of `thin_samples`, not something it sorts. Heap-backed harness data
is forgotten after assertions to keep unrelated collection destruction outside
the proof; the production functions and memory checks within them are unchanged.

The sample harness uses unwind 8 even with only two readings: Rust's internal
byte-swap loop also needs a sufficient bound. Unwind 4 failed an unwind assertion;
the bound was increased instead of disabling the check.

## Deferred after actual verifier runs

Broader prototypes for two-provider `reserve`, cooldown clearing/overwrites, multiple tiers and
`provider_blocked_until`, full `Policy::eligibility`, and up to six history
samples repeatedly exceeded the 120-second budget on Kani 0.68.0. Profiling
showed time in symbolic execution and heap-pointer simplification, before SAT
solving. Those prototypes are not shipped as permanently failing advisory
checks, and no successful proof is claimed for them. A later cut can investigate
library contracts or further decomposition without rewriting production behavior
solely to accommodate the verifier.

The existing proptest suite still covers provider routing, complete policy
eligibility and broad observation histories. Kani adds exhaustive checks within
the small domains above; it does not replace those tests. Inspect individual
harness results: a nonblocking CI job's overall status alone is not proof evidence.

## Local verification (2026-10-10)

All five retained harnesses verified with Kani 0.68.0 / CBMC 6.11.0 on Apple
Silicon, with unwind checks enabled. Measured proof times: period tiling ~30 s,
fraction/remaining ~4 s, rate-limit boundary ~10 s, ledger conservation ~1 s,
and two-sample thinning ~57 s. These are bounded proofs, not guarantees outside
the documented domains. The advisory CI job repeats the complete set on Linux.
