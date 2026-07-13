# Phase 08: Hermes packaging and documentation

Entry: full daemon integration passes. Exit: real Hermes 0.18.2 install/enable/setup/cron/update/remove and docs/bridge contracts pass.

## Tasks

- [Task 8.1: Finalize the reference bridge and public docs](../tasks/8.1-finalize-the-reference-bridge-and-public-docs.md)

Tasks are selected by dependency eligibility from task-manifest.json; list order is descriptive, not permission to skip dependencies.

## Phase gate

Run:

- `pytest -q tests/hermes_plugin_test.py tests/bridge_test.py`
- `cargo test --locked --test docs_contract_test`
- `cargo test --locked --all-targets`

Also verify every task in this phase has a complete handoff, no task is blocked/in-progress, forbidden-side-effect assertions pass, and CONTRACTS.md was not changed without an explicit plan revision. Record results in `../handoffs/phase-08.md`.
