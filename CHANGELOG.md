# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `dashboard --once` renders one frame as plain text and exits (works without
  a TTY; `--json` prints the snapshot), and `dashboard --ascii` draws with
  `*`/`>`/`#` and `+-|` borders (automatic when the locale is not UTF-8).
- `doctor` warns when `TERM` is unset/`dumb` or the locale is not UTF-8.

### Changed

- `dashboard` refuses to start without `config.toml` (same "run `powerqueue
  init`" message as `status`), or when stdin/stdout are not a terminal or
  `TERM` is unset/`dumb` (exit 2 with a plain error) instead of writing escape
  sequences into a pipe; terminal initialisation errors are reported with
  context.
- Attaching from the dashboard inside the powerqueue tmux server uses
  `switch-client` and keeps the dashboard running; nested attaches from
  another tmux server clear `$TMUX` so tmux accepts them.
- Budget gauges no longer hard-code a black background.

## [0.1.0] - 2026-10-01

### Added

- Project documentation: README, priority grammar, budget algorithm,
  troubleshooting, architecture.
- CI (fmt, clippy, test, doc, audit) and release workflows; Dependabot;
  issue and pull request templates.
- `justfile` with build, test, lint, doc, ci and dev-state recipes.
- End-to-end tests driving the real daemon with a fake Claude Code binary
  (`tests/fixtures/fake-claude.sh`) on a private tmux server.
- Workspace trust pre-seeding (`claude.trust_workspace`) and an always-on
  `Bash(powerqueue task *)` allow rule so unattended sessions never stall on
  the trust dialog or on the completion command.
- Account-wide rate-limit cooldowns: a `rate_limit` reported by one tier pauses
  every tier until the cooldown or the period reset.

- Linear integration: poll queued issues per team, state and label; move
  issues through workflow states; post progress comments.
- Manual tasks with `powerqueue add`.
- `PRIORITY.md` rules: criticality sections, scoring, per-ticket overrides,
  model mapping, optional Jev scoring; live reload.
- One git worktree and one tmux window per task; interactive Claude Code
  sessions with hooks reporting back through `powerqueue hook`.
- Crash recovery with backoff and `--resume` of the same session id.
- Token usage learning from Claude Code transcripts; CPU/RSS sampling.
- Budget pacing across the subscription period and rolling window, with
  per-tier shares, minimum criticality, relaxation, end game, safety margin,
  rate-limit cooldowns, calibration (`budget set-reset`, `budget set-observed`)
  and a cost estimator.
- Live dashboard (`powerqueue dashboard`), one-shot `status`, `task show`
  timelines, `task explain`.
- `powerqueue doctor` diagnostics with fix hints and `--fix`.
- Secrets in the OS keychain with a 0600 file fallback and
  `POWERQUEUE_SECRETS=file`.
- XDG paths with `POWERQUEUE_HOME` override; rotating JSON logs.

[Unreleased]: https://github.com/aleandros/powerqueue/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/aleandros/powerqueue/releases/tag/v0.1.0
