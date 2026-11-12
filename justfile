set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

# List the recipes
default:
    @just --list

# Run the terminal, or one of its subcommands: `just run server`, `just run ls`
run *args:
    cargo run -- {{ args }}

# Run the optimized build
run-release *args:
    cargo run --release -- {{ args }}

build:
    cargo build

build-release:
    cargo build --release

# Type-check everything without producing binaries
check:
    cargo check --all-targets

# Run the tests, optionally filtered: `just test windows_pipe`
test *args:
    cargo test --all-targets {{ args }}

# Lint the way CI does: any warning is an error
lint:
    cargo clippy --all-targets -- -D warnings

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

# Everything CI checks, in its order; run it before opening a PR
ci: fmt-check lint test
