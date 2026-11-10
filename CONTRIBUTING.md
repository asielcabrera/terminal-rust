# Contributing

## Requirements

- Rust (stable)
- Zig 0.15.2 (needed to build `libghostty-vt`)

## Workflow

1. Fork the repo and create a branch from `master`.
2. Make your change; run `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings` and `cargo test`.
3. Open a PR with a Conventional Commits title (`feat: ...`, `fix: ...`).

CI must pass before merging. Contributors are added to the README automatically after merge.

## Releases

- Stable: push a tag like `v0.2.0`.
- Beta: push a tag like `v0.2.0-beta.1` (published as a pre-release).
