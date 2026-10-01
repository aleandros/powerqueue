# powerqueue developer recipes. Run `just` to list them.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Isolated state for `just dev` / `just clean-state`. Lives under target/ so it
# is gitignored and removed by `cargo clean`. Never the real XDG directories.
dev_home := justfile_directory() / "target" / "dev-home"

export POWERQUEUE_SECRETS := "file"
export NO_COLOR := env("NO_COLOR", "1")

default:
    @just --list

# Debug build.
build:
    cargo build

# Run the test suite.
test *ARGS:
    cargo test --all-features {{ARGS}}

# rustfmt check + clippy with warnings as errors.
lint:
    cargo fmt --all --check
    cargo clippy --all-targets --all-features -- -D warnings

# Format the code.
fmt:
    cargo fmt --all

# Build docs with warnings as errors.
doc *ARGS:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features {{ARGS}}

# Everything CI runs, in order.
ci: lint test doc
    @echo "ci: ok"

# Run the binary against your real configuration.
run *ARGS:
    cargo run --quiet -- {{ARGS}}

# Run the binary against an isolated dev state in target/dev-home.
dev *ARGS:
    mkdir -p "{{dev_home}}"
    POWERQUEUE_HOME="{{dev_home}}" cargo run --quiet -- {{ARGS}}

# Live dashboard.
dash:
    cargo run --quiet -- dashboard

# Diagnostics.
doctor *ARGS:
    cargo run --quiet -- doctor {{ARGS}}

# Install into ~/.cargo/bin.
install:
    cargo install --path . --locked

# Remove the isolated dev state (target/dev-home). Refuses anything else.
clean-state:
    #!/usr/bin/env bash
    set -euo pipefail
    dir="{{dev_home}}"
    case "$dir" in
        */target/dev-home) ;;
        *) echo "refusing to remove '$dir': not a target/dev-home directory" >&2; exit 1 ;;
    esac
    if [ -e "$dir" ]; then
        rm -rf -- "$dir"
        echo "removed $dir"
    else
        echo "nothing to remove at $dir"
    fi
