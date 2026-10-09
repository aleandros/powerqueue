# AGENTS.md — working on powerqueue

This file is for AI agents and humans alike. Read it before changing code.

## What powerqueue is

A Rust CLI/daemon that turns Linear tickets, GitHub Issues (and manual tasks) into Claude Code
sessions. Each task gets its own git worktree and its own tmux window. The
daemon learns token/CPU/memory usage per task, restarts crashed sessions,
paces model usage across the subscription period (Fable is reserved for
critical work early in the period and released near the end), cleans up
when a task completes, and exposes a live dashboard plus a `doctor` command
that explains what is wrong and how to tune the algorithm.

## Layout

```
src/
  main.rs            thin entry point
  lib.rs             module tree + crate docs
  domain.rs          core types (Task, Session, TokenUsage, Provider, ModelTier (string-backed, provider inferred), ...)
  config.rs          config.toml + per-repo .powerqueue.toml; per-provider budgets, legacy key migration
  paths.rs           XDG directories; POWERQUEUE_HOME override
  secrets.rs         keychain (keyring) with 0600-file fallback; env wins
  logging.rs         tracing: stderr + rotating JSON file
  store/             SQLite (rusqlite, WAL): tasks, sessions, usage, resource_samples, events, commands, hook_events, kv, jev_scores
  linear/            GraphQL client + issue→task sync
  priority/          PRIORITY.md parser/evaluator + live reload
  jev.rs             TypeSafe Jev "score" client (optional scoring)
  github.rs          `gh api graphql` PR status for the PR watcher
  github/            REST issue client, intake and sync
  budget/            pure: period clock, ledger (Ledger::build over plain rows; one per provider: Ledgers), probe.rs (observed usage + UsageProbe),
                     cost estimator, model policy; io.rs is the only budget module that reads/writes the store (Ledger::load, observations)
  worktree.rs        git worktree ops (shell out to git)
  tmux.rs            tmux ops (shell out to tmux)
  service.rs         `powerqueue service`: systemd user unit / launchd agent rendering, parsing, systemctl/launchctl ops
  session/           agent.rs (AgentCli trait, agent_for, shared helpers), claude.rs, codex.rs, gemini.rs (one per CLI), binary.rs (`<provider>.binary` command templates), inbox.rs (container shim + inbox for `<provider>.shim`), launcher (prompt, launch.sh), transcript tailing, probes
  scheduler/         daemon loop (daemon.rs: store, tmux, git, Linear, GitHub; applies Effects) around a pure core:
                     transitions.rs (hooks, probes, crashes, rescoring, finalize), commands.rs (pause/resume/cancel/retry/model, the direct writes complete/hand-off/block),
                     launch.rs (LaunchPlanner, on_starting/on_launched, resume plan + prompt), review.rs (PR watcher),
                     lifecycle.rs (pick_next, cleanup_plan; cleanup_task is the git/tmux half)
  hook.rs            `powerqueue hook` (called by Claude Code hooks)
  dashboard/         ratatui TUI
  doctor.rs          diagnostics + tuning advice
  tune.rs            `powerqueue tune`: drafts dir, prompt, headless `claude -p` run, validate/diff/apply/undo
  cli/               clap definitions, context, output helpers, command handlers
tests/               integration tests (assert_cmd, wiremock, temp git repos, private tmux sockets);
                     fixtures/fake-claude.sh + e2e_daemon.rs drive the real daemon end to end
docs/                user docs (priority grammar, budget algorithm, troubleshooting)
```

## How a task flows

1. `linear::sync_issues` creates a `Task` (state `queued`) for each queued issue; `powerqueue add` does the same for manual tasks.
2. `priority::PriorityRules::evaluate` sets `criticality`, `score`, an optional *preferred* model, `skip` (→ `paused`).
3. `scheduler` picks the best task (`pick_next`), asks `budget::Policy::decide` for a model (or a `retry_at` when throttled). `task.model_override` (CLI only) and the rules' model are both preferences the policy may downgrade.
4. `worktree::Repo::add_worktree` creates `<worktree_root>/<slug>` on branch `pq/<slug>`; `repo.setup` commands run.
5. `session::Launcher::prepare` writes `<state>/tasks/<id>/{prompt.md,launch.sh,env}` plus whatever `session::agent_for(model.provider()).prepare` asks for (`settings.json` for Claude); `launch` runs the provider's `pre_launch` and opens a tmux window running `launch.sh`.
6. Claude Code hooks (`SessionStart`, `Stop`, `StopFailure`, `SessionEnd`, `Notification`, ...) call `powerqueue hook`, which stores the payload in `hook_events`.
7. The daemon drains hook events + tails the transcript JSONL for usage (dedupe by `message.id`), samples CPU/RSS via sysinfo, probes tmux panes.
8. Completion: Claude runs `powerqueue task complete <id> --summary ...` and/or prints `[[POWERQUEUE:DONE]]` (either suffices). Dead pane without completion ⇒ `crashed` ⇒ backoff ⇒ relaunch with `--resume <same session id>`. With `--pr <url>` the task goes `in_review` instead: session and slot released, worktree removed, branch kept; `scheduler::review` watches the PR via `gh` and re-queues a review round (same worktree path, `--resume` of the same session, `scheduler.review_prompt`) on conflict / failed required check / new review threads, or completes it when merged.
9. `cleanup_task`: push branch, remove worktree, close window (per config + repo overrides), update source issue state/labels, post comment.

## Conventions

- Rust 2024 edition, stable toolchain. `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test` must pass.
- Errors: `anyhow::Result` at boundaries, `thiserror` for typed errors users branch on. Always add context (`with_context`) naming the file/command involved.
- Logging: `tracing` with structured fields (`task = %task.key`, `session = %id`). Never log secrets. Important state changes also go to `store.log_event(...)` so `task show` and the dashboard can replay them.
- Shelling out: wrap `git`/`tmux`/`claude` in the dedicated modules; include stderr in errors; never use a shell for argument splitting except for user-configured `sh -c` commands.
- Time: `chrono::DateTime<Utc>` everywhere; stored as RFC 3339.
- Config: every field has a default; `deny_unknown_fields`; `Config::validate` reports problems in plain language.
- No `unwrap()` outside tests; `expect()` only for invariants with a message.
- Public functions get a doc comment stating behaviour and failure modes.
- Functional core, imperative shell: every task/session state change is made by a pure function in
  `scheduler/{transitions,commands,launch,review}.rs` that returns `Effect`s; `daemon.rs` only loads rows,
  calls those functions, persists what changed (`Daemon::commit`) and carries out the effects. Never set
  `task.state` / `session.state` in the daemon; add a transition (with a unit test) and, if the outside world
  must do something new, an `Effect` variant. The budget core (`period`, `ledger`, `probe`, `estimator`,
  `policy`) takes plain data; store access lives in `budget/io.rs`.
- Tests: unit tests next to the code; properties (`proptest`) next to them in `mod properties`,
  drawing from `src/strategies.rs`, plus the stateful model test in `scheduler/model.rs`;
  a failing property's seed under `proptest-regressions/` is committed with the fix; integration tests in `tests/` using `POWERQUEUE_HOME` + `POWERQUEUE_SECRETS=file` in a tempdir. Network via `wiremock`. tmux/git tests skip themselves with `which::which(...)` when the tool is not on PATH and use a private tmux socket (`-L powerqueue-test-<pid>-<random>`); see CONTRIBUTING.md.
- UX: output goes through `cli::output` helpers; colours respect `--no-color`/`NO_COLOR`; `--json` prints machine-readable output for status/task/budget/doctor.
- Keep the CLI surface in `cli/mod.rs` in sync with `docs/commands.md`; keep README.md focused on the overview and quick start.

## Working in parallel

Module ownership is coarse so agents can work on branches without conflicts:

| area | files |
|------|-------|
| linear + priority + jev | `src/linear/**`, `src/priority/**`, `src/jev.rs`, `src/cli/commands/{linear,priority}.rs` |
| runtime | `src/tmux.rs`, `src/worktree.rs`, `src/service.rs`, `src/session/**` (incl. `session/{agent,claude,codex,gemini}.rs`, the provider trait and its CLIs), `src/cli/commands/{attach,service}.rs` |
| budget + scheduler | `src/budget/**` (incl. `budget/probe.rs`, observed usage, `budget/io.rs`), `src/scheduler/**` (pure core + daemon), `src/github.rs`, `src/hook.rs`, `src/cli/commands/{run,budget,hook}.rs` |
| ux | `src/dashboard/**`, `src/doctor.rs`, `src/tune.rs`, `src/cli/commands/{init,status,add,task,logs,config,secrets,doctor,tune}.rs` |

Shared files (`domain.rs`, `config.rs`, `store/**`, `cli/mod.rs`, `Cargo.toml`) may be
extended but not reshaped; add, don't rename. If you must change a public
signature another area depends on, say so in your PR/report.

## Checklist before you finish

- [ ] `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
- [ ] New config keys documented in `docs/configuration.md` and have defaults
- [ ] New CLI flags documented in `docs/commands.md`
- [ ] Events logged for new state transitions
- [ ] A new or changed transition has a property (`mod properties` in its module) or an op in
      `scheduler/model.rs`, not only an example test; generators live in `src/strategies.rs`
- [ ] `doctor` knows about any new failure mode you introduced
