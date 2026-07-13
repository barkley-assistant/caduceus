# Phase 00: Scaffolding

Entry: empty Rust workspace. Exit: crate/module graph and locked toolchain compile cleanly.

## Tasks

- [Task 0.1: Create the Rust crate and module graph](../tasks/0.1-create-the-rust-crate-and-module-graph.md)

Tasks are selected by dependency eligibility from task-manifest.json; list order is descriptive, not permission to skip dependencies.

## Phase gate

Run:

- `cargo build --locked --all-targets`
- `cargo fmt --check`
- `cargo clippy --locked --all-targets -- -D warnings`

Also verify every task in this phase has a complete handoff, no task is blocked/in-progress, forbidden-side-effect assertions pass, and CONTRACTS.md was not changed without an explicit plan revision. Record results in `../handoffs/phase-00.md`.
