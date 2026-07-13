# Phase 07: Orchestration and system verification

Entry: every subsystem implementation complete. Exit: canonical tick, metadata/status, cancellation, and ten full-system scenarios pass.

## Tasks

- [Task 7.0: Define orchestration-owned types and dependency injection](../tasks/7.0-define-orchestration-owned-types-and-dependency-injection.md)
- [Task 7.1: Implement the single canonical tick](../tasks/7.1-implement-the-single-canonical-tick.md)
- [Task 7.3: Implement status and heartbeat inspection](../tasks/7.3-implement-status-and-heartbeat-inspection.md)
- [Task 7.4: Handle SIGINT and SIGTERM through cancellation](../tasks/7.4-handle-sigint-and-sigterm-through-cancellation.md)
- [Task 7.5: Full-system integration suite](../tasks/7.5-full-system-integration-suite.md)

Tasks are selected by dependency eligibility from task-manifest.json; list order is descriptive, not permission to skip dependencies.

## Phase gate

Run:

- `cargo test --locked --all-targets`
- `cargo fmt --check`
- `cargo clippy --locked --all-targets -- -D warnings`

Also verify every task in this phase has a complete handoff, no task is blocked/in-progress, forbidden-side-effect assertions pass, and CONTRACTS.md was not changed without an explicit plan revision. Record results in `../handoffs/phase-07.md`.
