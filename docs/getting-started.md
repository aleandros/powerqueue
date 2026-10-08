# Getting started

[Documentation](README.md) · [Project overview](../README.md)

## Requirements

Linux and macOS are supported; Windows is untested. Install git, tmux and
Claude Code, and authenticate the agent before starting the default setup.
`powerqueue doctor` checks the tools and credentials for enabled providers.
See [provider settings](configuration.md#providers) for optional Codex and
experimental Antigravity support.

Linear needs a personal API key. GitHub Issues needs a token for the configured
repository; PR watching separately requires an authenticated `gh` CLI. Jev
scoring is optional and uses its own API key. Store integration keys with
[`powerqueue secrets`](commands.md#config-and-secrets).

## Installation

One line, no toolchain needed. The script detects your OS and architecture and
drops the binary in `/usr/local/bin` (or `~/.local/bin` as a fallback):

```sh
curl -fsSL https://raw.githubusercontent.com/aleandros/powerqueue/main/install.sh | sh
```

**Update**: `powerqueue update` downloads the latest release for your
platform, verifies its SHA-256, checks that the new binary runs, and swaps it
in atomically, so a running daemon keeps working until you restart it
(`powerqueue service restart` when it runs as a service, otherwise
`powerqueue stop`, then `powerqueue run`). `powerqueue update --check` only
tells you whether a newer release exists (exit code 10 when it does, handy in
cron). Re-running the install script works too. Pin a version with
`powerqueue update --version v0.4.0` (or `POWERQUEUE_VERSION=v0.4.0` for the
script), or choose the directory with `POWERQUEUE_INSTALL_DIR=~/bin`.

Other ways:

```sh
# with a Rust toolchain
cargo install --git https://github.com/aleandros/powerqueue

# from a checkout
cargo install --path .
```

Pre-built binaries for Linux (x86_64, aarch64) and macOS (x86_64, arm64) are
attached to every [GitHub release](https://github.com/aleandros/powerqueue/releases),
with SHA-256 sums. Releases are cut automatically when `version` in
`Cargo.toml` changes on `main`.

Project site: <https://aleandros.github.io/powerqueue/>

## Linear

```sh
cd ~/code/my-repo
powerqueue init --team ENG          # keys, repo, Linear team, PRIORITY.md
powerqueue doctor                   # checks git/tmux/claude, keys, config
powerqueue run                      # daemon, foreground
```

## GitHub Issues

From the matching local checkout:

```sh
powerqueue init --no-linear
powerqueue secrets set github
powerqueue config set github.repository '"owner/repo"'
powerqueue config set github.enabled true
powerqueue github test
powerqueue github sync              # preview open issues labeled powerqueue
powerqueue run
```

To use both trackers, keep Linear enabled and add the GitHub token and settings.
See [GitHub configuration](configuration.md#github) for filters and optional lifecycle updates.

## Manual tasks

```sh
cd ~/code/my-repo
powerqueue init --no-linear
powerqueue add "Fix flaky test" -c high
powerqueue doctor
powerqueue run
```

Use `init --reconfigure` to change an existing installation. Each instance
manages one checkout; `--home DIR` keeps additional instances separate.

## Running unattended

Install git, tmux and the agent CLI on the host, authenticate the agent, and
clone your repository before running `init`. Choose a permission mode for
the work you intend to allow. The default `acceptEdits` can pause for shell
command approval; `auto` lets Claude Code classify commands for approval.
See [Claude settings](configuration.md#claude) for the available modes.

Use `powerqueue service install` to keep the daemon running after your terminal
closes. On a Linux server, `powerqueue service install --linger` also enables
running after logout and at boot. See [Operations](operations.md) for service
setup, updates to the service environment, secrets and logs.
