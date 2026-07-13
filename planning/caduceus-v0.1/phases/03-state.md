# Phase 03: Durable state and claims

Entry: shared queue types complete. Exit: state writes, claims, retry transitions, and operator reset are crash-safe and deterministic.

## Tasks

- [Task 3.1: Implement crash-safe StateStore](../tasks/3.1-implement-crash-safe-statestore.md)
- [Task 3.2: Create and release atomic claims](../tasks/3.2-create-and-release-atomic-claims.md)
- [Task 3.4: Enforce retry and terminal transitions](../tasks/3.4-enforce-retry-and-terminal-transitions.md)

Tasks are selected by dependency eligibility from task-manifest.json; list order is descriptive, not permission to skip dependencies.

## Phase gate

Run:

- `cargo test --locked --test state_store_test --test claim_test --test daemon_lock_test --test retry_test --test queue_reset_cli_test`
- `cargo test --locked --all-targets`

Also verify every task in this phase has a complete handoff, no task is blocked/in-progress, forbidden-side-effect assertions pass, and CONTRACTS.md was not changed without an explicit plan revision. Record results in `../handoffs/phase-03.md`.
