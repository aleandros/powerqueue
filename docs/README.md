# powerqueue documentation

[Project overview and quick start](../README.md)

## Setup and daily use

- [Getting started](getting-started.md): installation, updates, Linear, GitHub Issues, manual tasks and unattended use.
- [Command reference](commands.md): commands, flags and output behavior.
- [Configuration reference](configuration.md): defaults, providers, issue sources, prompts and repository overrides.
- [Containers](containers.md): run agents in per-task containers with command templates and the inbox shim.
- [Operations](operations.md): systemd/launchd services, credentials, paths and logs.
- [Troubleshooting](troubleshooting.md): symptoms, diagnostics and recovery.

## Scheduling and task behavior

- [Priority rules](priority.md): grammar, model preferences, simulation and tuning.
- [Budget pacing](budget.md): usage readings, cost estimates, provider limits and worked examples.
- [Task lifecycle](task-lifecycle.md): worktrees, completion, PR review, human replies and cleanup.

## Examples

- [Priority rules](examples/priority-rules.md): a complete example to adapt.
- [Prompt template](examples/prompt-template.md): placeholders for customizing agent instructions.

## Development

- [Architecture](architecture.md): modules, storage, state machines and the daemon loop.
- [Contributing](../CONTRIBUTING.md): development setup, tests and releases.
- [Changelog](../CHANGELOG.md): changes by release.

The [reference/](reference/) directory contains implementation research and
historical design notes. Use the configuration and command references above
for current behavior.
