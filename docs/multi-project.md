# One queue for several projects

[Documentation](README.md) · [Configuration](configuration.md)

One daemon shares concurrency and provider budgets across all execution projects.
A GitHub source maps explicitly to a project; a task retains that assignment and,
once work starts, its checkout identity through restart, retry and PR review.
The examples below use synthetic repositories and paths. Replace them with your
own checkouts and repositories before running commands.

## Register and inspect

Install and authenticate your agent CLI, git, tmux and the GitHub CLI as described
in [Getting started](getting-started.md). Start a new GitHub/manual installation
with `project register`, or use it to extend an existing `init` installation:

```sh
powerqueue project register api /srv/checkouts/api --github example/api
powerqueue project register web /srv/checkouts/web --github example/web
powerqueue project register worker /srv/checkouts/worker --github example/worker
powerqueue secrets set github
powerqueue config set scheduler.max_concurrent 2
powerqueue project list
powerqueue project list --json
powerqueue doctor --offline
powerqueue github test
powerqueue doctor
```

Each checkout must already exist and be a git repository. Registration writes the
project and source together, validates effective repository overrides, preserves
unrelated settings and comments, and requests reload when the daemon is running.
A new installation disables Linear. A named-only registry needs explicit
`add --project ID`; Linear intake requires a registered `default` project.
Existing `[repo]` and `[github]` settings retain their identities and behavior.

Repeat the same registration command to reconfigure a checkout. Unspecified
project settings, source filters and credentials remain unchanged. The command
rejects moving a checkout with pinned tasks or assigning an existing source to
another project. Deliberate source reassignment uses `config edit`, and unstarted
tasks use `task project`; active work is never silently moved. Registering an
existing disabled source leaves it disabled.

Use `--token-env API_GITHUB_TOKEN` with `--github` for separate credentials. Set
the variable in the daemon's service environment as well as your shell; its
value is never written into config. `--trusted-approver maintainer` restricts
approval to verified label events from that login; repeat it for more logins.
Supplying approvers replaces that source's list; omitting them preserves it.
Use `config edit` to clear an allowlist or change advanced source filters.

`project list --json` includes each project's effective repository, prompt,
cleanup, priority and override information, plus shared budget and concurrency
settings. Repository `.powerqueue.toml` files remain the place for per-project
setup commands and instructions. Their concurrency and budget settings cannot
enlarge the machine's shared limits. Worktrees use separate project directories.
Doctor checks source credentials/access, mappings, pinned checkouts, effective
settings, writable worktree roots, duplicate repositories and overlapping roots.
Orphan cleanup visits every project; isolation failures disable automatic
worktree cleanup until the paths are corrected.

## Approve, preview and operate

New sources require `agent:ready`. Apply it deliberately after reviewing the
issue's requested work; planning issues and ordinary category labels do not
approve execution. Title/body changes require review and reapproval. Existing
explicit labels are preserved. See [GitHub admission](configuration.md).

```sh
powerqueue github sync                 # read-only preview: key, project, admission
powerqueue github sync --apply         # import; still does not start sessions
powerqueue task show 'example/api#1'
powerqueue task explain 'example/api#1'
powerqueue task prompt 'example/api#1'
powerqueue run
```

In another terminal:

```sh
powerqueue status
powerqueue status --project web --all
powerqueue task list --project worker
powerqueue dashboard --project api
powerqueue dashboard --once --project api --json
powerqueue add 'Small maintenance task' --project worker
```

Status counts, dashboard slot counts and provider budgets always describe the
whole queue, even when the task table is filtered. Filters also accept removed
project IDs, so historical tasks remain inspectable. Source failures block that
source's future starts; healthy projects continue. Removing approval preserves
active sessions but blocks subsequent attempts and review starts.

## Repeatable disposable validation

With git, tmux and python3 installed, from the source checkout run:

```sh
cargo test --test projects
cargo test --test e2e_daemon three_project -- --nocapture
cargo test --test e2e_daemon execution_project -- --nocapture
```

The three-project scenario registers `api`, `web`, and `worker` using the CLI.
Each gets a temporary git repository, local bare origin and mocked GitHub API.
All use issue #1 and PR #7 to expose identity collisions. It checks approval-only
intake, a budget hold affecting all three projects, source failure isolation,
a shared slot despite local overrides, checkout/worktree/branch identity,
conflict-driven review with the original session, per-source closure and cleanup.
The other scenarios cover setup failure, crash/resume and daemon restart.
No live GitHub requests or agent subscriptions are involved. Missing prerequisites
skip e2e tests; confirm the test output contains a run, not a skip.

Rust properties and the stateful daemon/SQLite models cover three project IDs,
reassignment, approval changes, shared provider accounting and stale candidates.
Quint models three projects, two providers and shared slots/reservations; it
abstracts PR review, which is covered by Rust models and these e2e tests. Kani
checks bounded production budget arithmetic, periods, rate-limit boundaries and
sample thinning. Its shared-ledger harness checks three independently costed
reservations; it does not prove project routing or the entire daemon. Run:

```sh
cargo test --lib
npm --prefix spec run typecheck
npm --prefix spec test
npm --prefix spec run check
cargo test --example quint-replay -- --include-ignored
cargo kani --lib -Z unstable-options --harness-timeout 300s --output-format terse --jobs 2
```

See [spec/README.md](../spec/README.md) for installation and optional exhaustive
assignment/progress checks. Passing a simulation or replay is not a proof of the
implementation's equivalence to the specification.

## Opt-in live smoke test

Live-account validation is an explicit operator action, never part of the tests
above. Use three designated disposable repositories and separate clones, plus a
fresh instance home (for example `export POWERQUEUE_HOME=/tmp/pq-smoke-unique`).
Do not point this instance at production checkouts or share a service's home.

1. Register the three clones and their source mappings as above. Authenticate the
   real agent and both GitHub access paths (token and `gh`). Run doctor online.
   Verify each clone's `origin`, default branch and effective cleanup settings.
2. Create one tiny implementation issue per repository and one unapproved
   planning issue. Record their keys. Do not automate approval on issue creation.
3. Preview intake before approval; confirm the planning issue is absent. Review
   and label only the three implementation issues `agent:ready`, then preview
   again. Confirm each maps to its intended checkout using `project list` and
   task inspection after `github sync --apply`.
4. Set shared concurrency to one and choose a small appropriate provider budget
   with `config set`. Start `run` in a terminal. Observe the combined queue and
   each filtered view; verify only one live session at a time and no planning
   task starts. Record the task's branch, worktree and checkout.
5. Have each agent hand off its PR with `task complete --pr URL`. Verify the slot
   is released and the source issue stays open before merge. Request one review
   correction per PR; confirm it resumes in the same project and session. Merge
   only after reviewing the changes. Verify completion, project-local cleanup
   and source labels/comments (closure depends on `close_on_complete`).
6. Run `doctor`, inspect events and save the results outside version control.
   Stop this instance with `powerqueue stop`. Inspect any remaining worktrees,
   branches and tmux windows before removing the disposable home, clones and
   remote fixtures. Repeat with a fresh home and fresh issues for a clean run.

This procedure tests real permissions and agent behavior that mocks cannot
validate. It can create commits, push branches, use subscription budget and update
issues; the operator decides when to run it.
