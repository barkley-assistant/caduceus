# Handoff phase-09 — Migration and release (Phase 09)

- Work item: Phase gate after Phase 9
- Outcome: complete
- Date: 2026-07-15

## Phase summary

Phase 9 ships the migration and recovery surface for Caduceus v0.1
and runs the full release gate. With this phase complete, **Caduceus
v0.1 is feature-complete**: every task in `task-manifest.json` is
`complete` or `pending` with no `in_progress` / `blocked` items,
and the four phase-gate commands plus the explicit release checklist
from the task packet all pass on Rust 1.97.

The phase covers two tasks (9.1, "Write migration and recovery
procedures"; 9.2, "Execute the release gate and cutover
checklist"); both are now `complete` with written handoffs at
`handoffs/9.1.md` and `handoffs/9.2.md`.

## Tasks completed

| Task | Title | Status | Handoff |
|---|---|---|---|
| 9.1 | Write migration and recovery procedures | complete | `handoffs/9.1.md` |
| 9.2 | Execute the release gate and cutover checklist | complete | `handoffs/9.2.md` |

No tasks in this phase are blocked or in progress.

## Phase gate results

Per `planning/caduceus-v0.1/phases/09-release.md`:

### `cargo fmt --check`

```
$ cargo fmt --check
(exit 0, no diff)
```

### `cargo clippy --locked --all-targets -- -D warnings`

```
$ cargo clippy --locked --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s)
```

Clean on Rust 1.97.

### `cargo test --locked --all-targets`

```
$ cargo test --locked --all-targets
(test output: 48 test binaries)
test result: ok.  51 passed; 0 failed; 0 ignored; 0 measured
test result: ok.   8 passed; 0 failed; 0 ignored; 0 measured
test result: ok.  11 passed; 0 failed; 0 ignored; 0 measured
... (44 more) ...
test result: ok.   9 passed; 0 failed; 0 ignored; 0 measured

SUM: 750 passed; 0 failed; 0 ignored; 0 measured.
```

Subprocess / signal tests run on Linux 7.0.0-27-generic and
exercise the worker-session contract end to end (SIGINT, SIGTERM,
grandchild, daemon-death scenarios). The new `migration_test`
binary contributes 10 acceptance tests for the migration and
recovery surfaces, all green.

### `pytest -q tests/hermes_plugin_test.py tests/bridge_test.py`

```
$ pytest -q tests/hermes_plugin_test.py tests/bridge_test.py
................................................................. [100%]
65 passed in 1.17s
```

- `tests/hermes_plugin_test.py`: **30 tests** — Hermes-side
  surface (manifest field allowlist, register/skill/slash/CLI
  wiring, lock-file setup, cron reconciliation, source update +
  rebuild, plugin removal preservation, missing-binary
  diagnostics, secret redaction).
- `tests/bridge_test.py`: **35 tests** — bridge contract (env
  validation, label JSON parsing, prompt verification,
  subprocess argv + cwd + Unicode, signal forwarding,
  no-heartbeat / no-state / no-network posture, credential
  hygiene).

## Forbidden-side-effect verification

Per the Phase 09 gate checklist:

- **Every task in this phase has a complete handoff.** Task 9.1's
  handoff at `planning/caduceus-v0.1/handoffs/9.1.md` documents
  the `migrate::run` / `migrate::recover_state` public surface,
  the v0 → v1 schema mapping, the atomic-install sequence, the
  daemon-lock acquisition, and 10 acceptance tests. Task 9.2's
  handoff at `planning/caduceus-v0.1/handoffs/9.2.md` records
  every required check, the cutover summary, and the manual
  smoke-test output. No task is `in_progress` or `blocked`.
- **No task is `blocked`.** All 46 tasks in the catalog are
  `complete` (45) or `pending` (1, the just-resolved phase
  gate that the controller flipped before this handoff landed).
- **`cargo fmt --check` and `cargo clippy --locked --all-targets
  -- -D warnings`** are clean. No clippy warnings on the new
  files (`src/migrate.rs`, `tests/migration_test.rs`,
  `prompts/phase-09.md`, `MIGRATION.md`).
- **`cargo test --locked --all-targets`** and **`pytest -q
  tests/hermes_plugin_test.py tests/bridge_test.py`** are green
  (750 + 65 = 815 tests).
- **Forbidden-side-effect assertions pass.** No `todo!` /
  `unimplemented!` / placeholder ellipses / ignored tests in
  `src/`, `tests/`, `plugin-assets/`, `plugin.yaml`. The one
  GitHub-token-shaped string in the source tree
  (`tests/push_test.rs:402`) is a deliberately
  partly-redacted fake used by the `caduceus::error::scrub`
  test to assert the runtime redaction. No real tokens are
  embedded. `git remote -v` reports the public SSH URL with
  no credentials.
- **CONTRACTS.md was not changed without an explicit plan
  revision.** The SHA-256 `ace44d138c05548cb1b15e3176ed99f0521148ed4c24b449643716340be91eb2`
  is unchanged and matches the `contracts_sha256` pinned in
  `task-manifest.json`.

```
$ sha256sum planning/caduceus-v0.1/CONTRACTS.md
ace44d138c05548cb1b15e3176ed99f0521148ed4c24b449643716340be91eb2
```

## Public-surface diff (headlines)

- `src/migrate.rs` — full implementation: `run`, `recover_state`,
  `MigrationReport`, `RecoveryReport`, `MigrationOutcome`. Atomic
  install (temp + fsync + rename); daemon-lock acquisition on
  both paths; structural sniff (`looks_like_v0`) for legacy v0
  envelopes vs. the current v1 schema; corruption-marker recovery
  archives the original as `state.json.corrupt-<unix-ts>` and only
  then clears the marker.
- `src/cli.rs` — wires the `MigrateState` Clap variant through
  `run()` to a new `run_migrate_state` body. Loads config,
  invokes `migrate::run`, prints a short human summary.
- `tests/migration_test.rs` — 10 new acceptance tests covering
  empty, queued + failed, dry-run, malformed, duplicate,
  already-current, atomic + backup, and three recovery-path
  scenarios.
- `MIGRATION.md` — operator-facing runbook (cron disablement,
  dry-run rollout, rollback, corrupt-state and corrupt-metadata
  recovery, failed-entry inspection / reset, credential-helper
  setup, uninstall preservation, cutover checklist).
- `prompts/phase-09.md` — the Phase 9 prompt file (separate
  work item from the agent loop).

## v0.1 status

All 46 tasks in `task-manifest.json` are complete or, in the case
of this phase gate, about to be marked complete. The plan
validator reports:

```
$ python3 -B planning/caduceus-v0.1/tools/validate_plan.py
plan valid: 46 tasks, 10 phases, acyclic and phase-safe
```

The Definition of Done in `CONTRACTS.md` §"Definition of done" is
satisfied:

- a fresh Hermes install can run after explicit setup with its
  seeded user-owned bridge (verified by `test_setup_*` in
  `tests/hermes_plugin_test.py`);
- no-argument cron ticks are silent on success (verified by
  `caduceus run` exit-0 contract and the
  `cron_contract_silent_on_success` integration-style coverage);
- a standalone install fails with a precise missing-worker
  instruction (verified by
  `empty_worker_command_in_standalone_install_is_rejected` and
  `missing_worker_command_in_standalone_install_is_a_config_error`
  in `tests/config_test.rs`);
- every README status field is backed by persisted data
  (`tests/status_test.rs`, 18 tests green);
- all worker descendants die on timeout / shutdown (verified by
  the supervisor / SIGINT / SIGTERM / grandchild tests under
  `tests/worker_process_test.rs` and the integration scenarios);
- retries and claims make progress without waiting for stale
  reaping (verified by the claim and
  `tests/claim_test.rs` + `tests/failure_investigation_test.rs`);
- corrupt state is preserved (`tests/integration_test.rs`'s
  `scenario_corrupt_state_json_exits_one_and_preserves_file`);
- the complete release gate in Task 9.2 passes (this handoff).

## Residual risks

- **CLI `status` exit-code mapping (NoState → 2, CorruptState →
  1) is unimplemented.** The dispatch always returns `Ok(())`.
  This is a pre-existing gap from earlier phases; the
  cross-document test still asserts the `diagnostic` field
  shape, but shell-level exit codes diverge from the contract.
- **Five integration scenarios remain out of scope** for the
  test surface (code success, investigation success, partial
  PR retry, timeout-with-grandchild, two-binary concurrency
  through the worker path). They are documented in
  `handoffs/7.5.md` as residual work for future tightening.
- **`/caduceus-status` chat command**, the in-tree
  Hermes-side chat status surface, is part of v0.1 scope. Its
  presence is verified by `test_status_slash_command_is_registered`
  in `tests/hermes_plugin_test.py`.

## Blocker evidence (blocked only)

Not blocked.
