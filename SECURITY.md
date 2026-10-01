# Security policy

## Reporting a vulnerability

Please do not open a public issue. Email **edgar.cabrera@pm.me** with the
subject `powerqueue security`, or use GitHub's private vulnerability reporting
on the repository if it is enabled. Include steps to reproduce, the version
(`powerqueue --version`) and your platform.

You will get an acknowledgement within 72 hours and a fix or mitigation plan
within 14 days for confirmed issues. Credit is given in the changelog unless
you prefer otherwise.

Only the latest release is supported.

## How powerqueue handles secrets

- API keys (Linear, Jev) are read from the environment first
  (`LINEAR_API_KEY`, `JEV_API_KEY`), then from the OS credential store
  (macOS Keychain, Secret Service, Windows Credential Manager) under the
  service name `powerqueue`, then from `<config>/secrets.toml`, which is
  written with mode 0600 and only used when no credential store is available
  or `POWERQUEUE_SECRETS=file` is set.
- Secrets are never written to logs, events, the database or `--json` output.
  `secrets list` shows presence and origin, not values; where a value must be
  shown (`init` confirmation) it is masked to the first four and last two
  characters.
- `config.toml` and the per-task `env` file are written with mode 0600.
- The Linear key is sent only to `linear.endpoint`; the Jev key only to
  `priority.jev.endpoint`. Both default to the vendors' HTTPS endpoints and
  can be pointed at mocks for tests.
- Claude Code sessions run with the permission mode in `claude.permission_mode`.
  `bypassPermissions` lets the agent run any command in the worktree without
  asking; use it only on machines and repositories where that is acceptable.
  `repo.setup` and `cleanup.run` commands execute with `sh -c` as your user.

## What is in scope

- Leaking a stored key through logs, output, files or network.
- Executing commands outside the task worktree without configuration asking
  for it.
- Privilege or permission escalation through hook or command handling.
- Bugs that let a crafted Linear issue (title, description, labels) inject
  shell commands or alter configuration.

Prompt injection through ticket content influencing what Claude does inside
its own worktree is a known limitation of agentic coding tools, not a
powerqueue vulnerability; review branches before merging.
