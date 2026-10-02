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
git clone https://github.com/aleandros/powerqueue
cd powerqueue
just build
just test
```

## `just` recipes

| Recipe | What it runs |
|--------|--------------|
| `just build` | `cargo build` |
| `just test` | `cargo test --all-features` with `POWERQUEUE_SECRETS=file` and `NO_COLOR=1` |
| `just lint` | `cargo fmt --all --check` and `cargo clippy --all-targets --all-features -- -D warnings` |
| `just fmt` | `cargo fmt --all` |
| `just doc` | `cargo doc --no-deps --all-features` with warnings as errors |
| `just ci` | everything CI runs, in order |
| `just run <args>` | `cargo run -- <args>` against your real config |
| `just dev <args>` | same, but with `POWERQUEUE_HOME` pointing at `target/dev-home` |
| `just dash`, `just doctor` | shortcuts for `run dashboard` / `run doctor` |
| `just install` | `cargo install --path . --locked` |
| `just clean-state` | removes `target/dev-home` only, never your real state |

## Tests

- Unit tests sit next to the code (`#[cfg(test)] mod tests`). Keep pure
  functions pure so they can be tested without I/O (`pick_next`,
  `interpret_hook`, `PriorityRules::parse`, `Policy::decide`).
- Integration tests live in `tests/`. `cli_smoke.rs` and `budget_cli.rs`
  drive the binary with `assert_cmd`; `priority_rules.rs`, `linear_client.rs`,
  `jev_client.rs`, `tmux_integration.rs` and `worktree_integration.rs` use the
  library API directly. Every CLI test gets its own temp dir and sets
  `POWERQUEUE_HOME` to it, plus `POWERQUEUE_SECRETS=file` so the OS keychain
  is never touched, and removes `NO_COLOR`, `LINEAR_API_KEY` and
  `JEV_API_KEY` from the environment.
- Network is mocked with `wiremock`; the client tests build `LinearClient`
  / `JevClient` with the mock server's URI (the same values you would put in
  `linear.endpoint` and `priority.jev.endpoint`).
- tmux tests use a private socket (`Tmux::new("tmux", Some("powerqueue-test-<pid>-<random>"))`,
  i.e. `tmux -L ...`) so they never see the developer's sessions, and a drop
  guard kills that server (and removes the socket file) when done. They skip
  themselves with `which::which("tmux")` when tmux is not on `PATH`; there is
  no shared `tests/common.rs`.
- Git tests create a throwaway repo in the temp dir with one commit and, where
  pushing matters, a local bare `origin`. They skip when `git` is missing.
- Prefer table-driven tests for parsers and the state machine.

Run one test with `cargo test name_of_test -- --nocapture`. Set
`RUST_LOG=powerqueue=debug` for stderr logging in tests.

### End-to-end test

`tests/fixtures/fake-claude.sh` stands in for Claude Code. It accepts the
flags the launcher passes, fires the command hooks declared in the
`--settings` file with realistic JSON payloads, appends assistant lines with
`message.usage` to the transcript JSONL under `$CLAUDE_CONFIG_DIR`, commits a
file in the worktree and calls `powerqueue task complete` through
`$POWERQUEUE_BIN`. `FAKE_CLAUDE_MODE` selects the behaviour: `complete`
(default), `crash-once` (exit 1 on the first run, finish on the resumed run),
`idle` (end the turn without a marker and wait) or `ratelimit` (report a
`StopFailure` with `rate_limit`). `FAKE_CLAUDE_STATE_DIR` holds its run
counters; the script needs `python3`.

`tests/e2e_daemon.rs` writes a `config.toml` that points `claude.binary` at
the fixture, passes the mode through `[claude.env]`, uses a private tmux
socket (`pq-e2e-<pid>-<random>`), `tick_secs = 1` and
`restart_backoff_secs = [1]`, then runs the real `powerqueue run` daemon
against a throwaway git repo and polls `status --json` / `task show --json`
until the task reaches the expected state. It covers completion with usage
and cleanup, crash + `--resume`, and a blocked session reaching
`needs_attention`. It skips itself when `tmux`, `git` or `python3` is
missing.

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

## Releasing

Releases are automatic:

1. Bump `version` in `Cargo.toml`, add the entry to `CHANGELOG.md` under that
   version, and merge to `main`.
2. `.github/workflows/tag.yml` tags the commit `v<version>` and starts
   `release.yml`, which builds Linux (x86_64, aarch64) and macOS (x86_64,
   arm64) binaries, packages them as `powerqueue-<target>.tar.gz` with SHA-256
   sums, and publishes a GitHub release with the changelog section as notes.
3. `install.sh` (and users re-running the one-liner) picks up the latest release.

No personal token is needed: the tag workflow starts the release with
`workflow_dispatch`, which GitHub allows from the default `GITHUB_TOKEN`.
To re-run a release by hand: `gh workflow run release.yml --ref v<version>`.

The landing page lives in `site/` and deploys through `pages.yml` on every
push that touches it.

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
