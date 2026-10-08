## What

<!-- One or two sentences. Link the issue: Fixes #123 -->

## Why

<!-- The problem this solves, or the behaviour it changes. -->

## How to verify

<!-- Commands, config, or a PRIORITY.md snippet a reviewer can try. -->

## Checklist

- [ ] `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` pass
- [ ] New config keys are documented in `docs/configuration.md` and have defaults
- [ ] New CLI flags are documented in `docs/commands.md` (`src/cli/mod.rs` in sync)
- [ ] Events are logged for new state transitions
- [ ] `doctor` knows about any new failure mode introduced
- [ ] `CHANGELOG.md` updated under `Unreleased`
- [ ] Public signature changes that other areas depend on are called out below

## Notes for reviewers

<!-- Trade-offs, follow-ups, anything you are unsure about. -->
