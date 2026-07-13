# Phase 09: Migration and release

Entry: all implementation and Hermes gates complete. Exit: migration, release, security, MSRV, and disposable-repository dry-run gates pass.

## Tasks

- [Task 9.1: Write migration and recovery procedures](../tasks/9.1-write-migration-and-recovery-procedures.md)
- [Task 9.2: Execute the release gate and cutover checklist](../tasks/9.2-execute-the-release-gate-and-cutover-checklist.md)

Tasks are selected by dependency eligibility from task-manifest.json; list order is descriptive, not permission to skip dependencies.

## Phase gate

Run:

- `cargo fmt --check`
- `cargo clippy --locked --all-targets -- -D warnings`
- `cargo test --locked --all-targets`
- `pytest -q tests/hermes_plugin_test.py tests/bridge_test.py`

Also verify every task in this phase has a complete handoff, no task is blocked/in-progress, forbidden-side-effect assertions pass, and CONTRACTS.md was not changed without an explicit plan revision. Record results in `../handoffs/phase-09.md`.
