# Handoff phase-08 — Hermes packaging and documentation (Phase 08)

- Work item: Phase gate after Phase 8
- Outcome: complete
- Date: 2026-07-15

## Phase summary

Phase 8 finalized the public shipping surface of Caduceus v0.1: the
canonical Python bridge, the cross-document fixtures, and the public
documentation. The phase covers exactly one task (8.1, "Finalize the
reference bridge and public docs"), which is now `complete` with a
written handoff at `handoffs/8.1.md`.

## Tasks completed

| Task | Title | Status | Handoff |
|---|---|---|---|
| 8.1 | Finalize the reference bridge and public docs | complete | `handoffs/8.1.md` |

No tasks in this phase are blocked or in progress.

## Phase gate results

The three phase-gate commands (per `planning/caduceus-v0.1/phases/08-hermes.md`)
all pass on Rust 1.97 with the project's `--locked` toolchain:

### `pytest -q tests/hermes_plugin_test.py tests/bridge_test.py`

```
65 passed in 1.12s
```

- `tests/hermes_plugin_test.py`: **30 tests** (Hermes-side surface —
  manifest field allowlist, register/skill/slash/CLI wiring, lock-file
  setup, cron reconciliation, source update + rebuild, plugin removal
  preservation, missing-binary diagnostics).
- `tests/bridge_test.py`: **35 tests** (bridge contract — env
  validation, label JSON parsing, prompt verification, subprocess
  argv + cwd + Unicode, signal forwarding, no heartbeat/state/result
  writes, credential hygiene posture).

### `cargo test --locked --test docs_contract_test`

```
running 16 tests
... (16 ok) ...
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Sixteen cross-document tests pin the v0.18.2 Hermes loader contract
across `plugin.yaml`, `README.md`, `skills/caduceus/SKILL.md`,
`plugin-assets/worker-bridge.py`, `src/config.rs`, `src/worker.rs`,
and `tests/hermes_plugin_test.py`.

### `cargo test --locked --all-targets`

```
test result: ok. 740 passed; 0 failed; 0 ignored; 0 measured
```

Across the daemon's `src/`, every `tests/*.rs` integration test
binary, and the new `tests/docs_contract_test.rs`. No regressions in
the previously passing 7.5 integration suite, the 7.3 status suite,
the 4.1 cron CLI suite, the 3.3 finalize-idempotent suite, or any
other prior work.

## Forbidden-side-effect verification

Per the Phase 08 gate checklist:

- **Every task in the phase has a complete handoff.** Task 8.1's
  handoff at `planning/caduceus-v0.1/handoffs/8.1.md` lists every
  file changed, every public signature affected, the test matrix
  added, the exact commands run, and the residual risks. No task is
  `in_progress` or `blocked`.
- **CONTRACTS.md is unchanged.** `sha256sum
  planning/caduceus-v0.1/CONTRACTS.md` returns the same digest as
  the `contracts_sha256` pinned in `task-manifest.json`:

  ```
  ace44d138c05548cb1b15e3176ed99f0521148ed4c24b449643716340be91eb2
  ```

- **`CONTRACT_REVISIONS.md` was not edited.** No revision was needed
  during the phase.
- **`archive/full-reviewed-plan.md`** is unchanged. Its digest remains
  the authority for the original plan.
- **No task is `blocked`.**
- **`cargo fmt --check` and `cargo clippy --locked --all-targets
  -- -D warnings`** are clean. No clippy warnings on the new files
  (`src/fixtures.rs`, `tests/docs_contract_test.rs`).

## Public-surface diff (headlines)

- `plugin-assets/worker-bridge.py` — canonical reference bridge.
  Strict env validation (one-line diagnostic, no value echo), JSON
  label parsing (legacy CSV shape rejected), prompt verification,
  `subprocess.run` with argument arrays (never `shell=True`),
  `invoke_harness` as the only user-editable hook, exit-code
  propagation, no heartbeats / state writes / network.
- `tests/bridge_test.py` — 35 pytest tests pinning the above
  contract end-to-end (including a deterministic Python harness
  fixture and subprocess-level signal propagation through a fresh
  process group).
- `tests/docs_contract_test.rs` — 16 Rust tests pinning the v0.18.2
  Hermes loader allowlist, the canonical Config/worker-env/allowlist
  lists, the negative-fixture forbidden set, and operator-facing
  documentation rules (lifecycle opt-in, pre-clones, PAT vs. SSH,
  standalone `worker_command`, etc.).
- `src/fixtures.rs` — single source of truth for the cross-document
  fixtures, re-exported via `pub mod fixtures;` from `src/lib.rs`.
- `plugin.yaml` — trimmed comment, explicit empty `requires_env`.
- `README.md` — adds Retry Semantics, Investigation vs. Code,
  Dry-Run Behavior, State Recovery Procedure, Session Transcripts;
  removes references to the legacy plugin/ subdirectory shape.
- `skills/caduceus/SKILL.md` — adds Investigation vs. Code, Retry
  Budget, State Recovery; documents v0.18.2 explicitly; preserves
  the opt-in plugin-skill contract.
- `__init__.py` `_cli_doctor` — prints file paths, opt-in flag,
  gateway requirement, and the full install/update/remove
  lifecycle.
- `plugin-assets/caduceus-pulse.sh` — header documents gateway/
  managed-cron requirement, `exec` semantics, and cron-remove
  ownership.

## Blocker evidence (blocked only)

Not blocked.

## Residual risks

- The cross-document test parses the bridge's `REQUIRED_ENV_VARS`
  tuple and the manifest allowlist set via heuristic matching
  (`trimmed.starts_with("REQUIRED_ENV_VARS:")` and brace-counted
  Python sets). A future refactor that changes the literal shape of
  those constructs would require a test update — but the changes
  would themselves require deliberate edits to the contract surface.
- The bridge test harness (`tests/fixtures/bridge_harness.py`) is
  installed through a Python `bin/opencode` wrapper that imports
  the harness module. This requires Python on PATH inside the test
  subprocess environment, which is universally satisfied on the
  tier-1 platforms but breaks down on hosts without an interpreter
  on PATH. A future tightening could replace the wrapper with a
  Rust harness.
- `requires_env: []` in `plugin.yaml` is part of the v0.18.2
  supported field list but Caduceus does not actually require any
  environment variables at plugin load time; we keep the entry
  explicit so the contract is unambiguous in the manifest.
- The `tests/bridge_test.py::test_signal_forwarded_to_harness_via_subprocess`
  test sends SIGINT to the bridge's full process group via
  `start_new_session=True`. In a real daemon tick the worker
  supervisor places the harness in its own process group with a
  dedicated leader; we test the same shape. A future hardening
  could spawn the bridge through the same `worker_supervisor`
  exercise path.
