# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Project documentation: README, priority grammar, budget algorithm,
  troubleshooting, architecture.
- CI (fmt, clippy, test, doc, audit) and release workflows; Dependabot;
  issue and pull request templates.
- `justfile` with build, test, lint, doc, ci and dev-state recipes.

## [0.1.0] - 2026-10-01

### Added

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

[Unreleased]: https://github.com/edgar/powerqueue/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/edgar/powerqueue/releases/tag/v0.1.0
