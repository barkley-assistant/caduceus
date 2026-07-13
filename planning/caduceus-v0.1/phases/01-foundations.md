# Phase 01: Foundational contracts

Entry: Phase 00 gate complete. Exit: configuration, shared domain/error/meta types, voice validation, worker-result schema, and root Hermes adapter contracts coexist without cycles.

## Tasks

- [Task 0.2: Implement and validate the Hermes adapter](../tasks/0.2-implement-and-validate-the-hermes-adapter.md)
- [Task 1.1: Parse and validate Config](../tasks/1.1-parse-and-validate-config.md)
- [Task 1.2: Resolve GitHub authentication](../tasks/1.2-resolve-github-authentication.md)
- [Task 1.3: Resolve config files and environment overrides](../tasks/1.3-resolve-config-files-and-environment-overrides.md)
- [Task 1.4: Initialize structured logging safely](../tasks/1.4-initialize-structured-logging-safely.md)
- [Task 1.5: Implement the unified error hierarchy](../tasks/1.5-implement-the-unified-error-hierarchy.md)
- [Task 1.6: Validate the worker command and runtime prerequisites](../tasks/1.6-validate-the-worker-command-and-runtime-prerequisites.md)
- [Task 3.0: Implement validated queue data types](../tasks/3.0-implement-validated-queue-data-types.md)
- [Task 5.3: Parse and validate worker results](../tasks/5.3-parse-and-validate-worker-results.md)
- [Task 6.6: Enforce the public-voice rule](../tasks/6.6-enforce-the-public-voice-rule.md)
- [Task 7.2: Persist complete daemon metadata](../tasks/7.2-persist-complete-daemon-metadata.md)

Tasks are selected by dependency eligibility from task-manifest.json; list order is descriptive, not permission to skip dependencies.

## Phase gate

Run:

- `cargo test --locked --all-targets`
- `pytest -q tests/hermes_plugin_test.py`
- `cargo fmt --check`
- `cargo clippy --locked --all-targets -- -D warnings`

Also verify every task in this phase has a complete handoff, no task is blocked/in-progress, forbidden-side-effect assertions pass, and CONTRACTS.md was not changed without an explicit plan revision. Record results in `../handoffs/phase-01.md`.
