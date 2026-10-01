# Contributing to powerqueue

Thanks for helping. This file covers the mechanics; [AGENTS.md](AGENTS.md)
covers the code conventions and module ownership, and
[docs/architecture.md](docs/architecture.md) explains how things fit.

## Setup

Requirements: stable Rust (the toolchain is pinned in `rust-toolchain.toml`,
with `rustfmt` and `clippy`), git, tmux ≥ 3.2, and [`just`](https://github.com/casey/just)
for the recipes below. Claude Code is only needed to run real sessions, not to
build or test.

```sh
git clone https://github.com/edgar/powerqueue
cd powerqueue
just build
just test
```

## `just` recipes

| Recipe | What it runs |
|--------|--------------|
| `just build` | `cargo build` |
| `just test` | `cargo test --all-features` with `POWERQUEUE_SECRETS=file` |
| `just lint` | `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings` |
| `just fmt` | `cargo fmt` |
| `just doc` | `cargo doc --no-deps` with warnings as errors |
| `just ci` | everything CI runs, in order |
| `just run <args>` | `cargo run -- <args>` against your real config |
| `just dev <args>` | same, but with `POWERQUEUE_HOME` pointing at `target/dev-home` |
| `just dash`, `just doctor` | shortcuts for `run dashboard` / `run doctor` |
| `just install` | `cargo install --path .` |
| `just clean-state` | removes `target/dev-home` only, never your real state |

## Tests

- Unit tests sit next to the code (`#[cfg(test)] mod tests`). Keep pure
  functions pure so they can be tested without I/O (`pick_next`,
  `interpret_hook`, `PriorityRules::parse`, `Policy::decide`).
- Integration tests live in `tests/` and drive the binary with `assert_cmd`.
  Every test gets its own temp dir and sets `POWERQUEUE_HOME` to it, plus
  `POWERQUEUE_SECRETS=file` so the OS keychain is never touched.
- Network is mocked with `wiremock`; point `linear.endpoint` and
  `priority.jev.endpoint` at the mock server.
- tmux tests use a private socket (`tmux.socket_name = "pq-test-<pid>"`) so
  they never see the developer's sessions, and kill that server when done.
  They are skipped when `tmux` is not on `PATH` (`tests/common.rs::tmux_available()`).
- Git tests create a throwaway repo in the temp dir with one commit and a
  local bare "remote".
- Prefer table-driven tests for parsers and the state machine.

Run one test with `cargo test name_of_test -- --nocapture`. Set
`RUST_LOG=powerqueue=debug` for stderr logging in tests.

## Pull requests

Before you open one, go through the checklist from AGENTS.md:

- [ ] `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` pass
- [ ] new config keys are documented in README.md and have defaults
- [ ] new CLI flags are documented in README.md (and `src/cli/mod.rs` stays in sync)
- [ ] events are logged for new state transitions
- [ ] `doctor` knows about any new failure mode you introduced
- [ ] `CHANGELOG.md` has an entry under `Unreleased`

Keep PRs focused on one area where possible (see the ownership table in
AGENTS.md). If you change a public signature another area depends on, say so
in the PR description.

## Commit style

Short imperative subject (≤ 72 chars), optionally prefixed with the area:
`budget: relax Fable one level after 50% of the period`. Body explains why,
not what. Reference issues with `Fixes #123`. One logical change per commit;
squash fixups before review.

## Reporting bugs and proposing features

Use the issue templates. For bugs, include `powerqueue --version` and
`powerqueue doctor --offline --json` with secrets removed. For features,
describe the problem first; the solution can come later.

## Security

Do not open public issues for vulnerabilities. See [SECURITY.md](SECURITY.md).

## License

By contributing you agree that your contributions are licensed under the MIT
license in [LICENSE](LICENSE).
