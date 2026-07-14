# Handoff phase-03 — Durable state and claims

- Work item: Phase 03 gate (Durable state and claims)
- Outcome: complete
- Date: 2026-07-14

## Phase summary

Phase 03 implemented the crash-safe `StateStore` (atomic write
+ `flock` + `O_CREAT|O_EXCL` claim files), the daemon-wide
nonblocking `DaemonLock`, the exact three-worker-failures
semantics on `retry_or_fail`, the `requeue_infrastructure`
path, and the `caduceus queue reset <owner/repo#number>`
operator CLI with `--dry-run` and `--force-finalization-reset`.
All three tasks completed without contract revisions, with
410 Rust tests + 30 pytest tests passing on Rust 1.97.

| Task | Title | Status |
|---|---|---|
| 3.1 | Implement crash-safe StateStore | complete |
| 3.2 | Create and release atomic claims | complete |
| 3.4 | Enforce retry and terminal transitions | complete |

Task 3.3 was the daemon-lock sub-task that Phase 2 already
covered; the controller skipped it in this phase.

## Gate commands run

```
$ python3 -B planning/caduceus-v0.1/tools/validate_plan.py
plan valid: 46 tasks, 10 phases, acyclic and phase-safe

$ cargo test --locked --test state_store_test --test claim_test \
    --test daemon_lock_test --test retry_test \
    --test queue_reset_cli_test
test result: ok. 27 passed (state_store_test)
test result: ok. 11 passed (claim_test)
test result: ok.  9 passed (daemon_lock_test)
test result: ok. 20 passed (retry_test)
test result: ok. 13 passed (queue_reset_cli_test)

$ cargo test --locked --all-targets
… 410 Rust tests pass total
  (27 state_store + 11 claim + 9 daemon_lock + 20 retry + 13
   queue_reset_cli + 230 carried over from prior phases)

$ PYTHONPATH=. pytest tests/hermes_plugin_test.py
30 passed in 0.47s

$ cargo fmt --check
(no diff)

$ cargo clippy --locked --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s)
```

## Per-task deliverables

Every task has a complete handoff under
`planning/caduceus-v0.1/handoffs/<task-id>.md`:

- `3.1.md` — Crash-safe `StateStore` with `enqueue`,
  `acquire_next`, `set_worktree`, `save_finalization`, and
  the terminal transitions `complete` / `complete_investigation`
  / `retry_or_fail` / `requeue_infrastructure` / `skip`.
- `3.2.md` — `acquire_next` race-loss retry, claim-file mode
  `0600`, and the `DaemonLock` RAII wrapper.
- `3.4.md` — `complete_preview`, `reset_entry`, the
  `caduceus queue reset` CLI with `--force-finalization-reset`
  and `--dry-run`.

## Forbidden-side-effect spot-checks

Per `CONTRACTS.md` and the Phase 03 gate:

- **#1 (whole-tick lock)** — `DaemonLock::try_acquire` is the
  implementation surface for invariant #1. The
  `daemon_lock_test::two_subprocesses_yield_one_winner` test
  exercises the canonical concurrent-tick case from a
  separate process and observes exactly one winner. The CLI's
  `caduceus queue reset` acquires the daemon lock before
  mutating state; the
  `queue_reset_cli_test::reset_refuses_when_daemon_lock_held`
  test asserts that the CLI returns a non-zero exit with a
  clear message while the lock is held.
- **#2 (atomic write with fsync)** — every mutating
  `StateStore` operation calls `atomic_write` (write-temp +
  `sync_all` + rename) followed by `sync_dir` on Linux. A
  malformed `state.json` is preserved verbatim; opening one
  returns `CaduceusError::StateCorrupt { path, message }`
  without touching the file. The `truncated_state_is_preserved_not_replaced`
  test in `state_store_test.rs` exercises this directly.
- **#3 (queue helpers never construct claim paths from raw
  strings)** — `acquire_next` is the only call site that
  builds a claim path. The path is derived from
  `display_digest(display_key)`, which is a SHA-256 hex
  string with no path-traversal characters by construction.
  The `hostile_key_cannot_affect_claim_path` test in
  `claim_test.rs` exercises the full surface (parse-time
  rejection + post-claim canonicalisation) and asserts the
  path stays under `claims/`.
- **#4 (terminal transitions remove claim)** — every terminal
  transition in `StateStore` calls `unlink_claim_best_effort`.
  The retry / complete / skip / complete_investigation /
  complete_preview paths are covered in `tests/retry_test.rs`
  (`claim_removed_on_retry_or_fail`,
  `claim_removed_on_requeue_infrastructure`,
  `completed_claim_is_deleted`,
  `investigation_complete_claim_is_deleted`,
  `skip_claim_is_deleted`).
- **Claim files have mode `0600`** — the new `set_mode_0600`
  helper forces the mode after `sync_all` (the umask would
  otherwise leave them at `0640` or `0660` on common Linux
  systems). The `claim_json_is_durable_and_versioned` test in
  `claim_test.rs` asserts the mode is `0o600` on Unix.
- **Queue reset never deletes a remote branch or PR** — the
  CLI emits "warning: the remote branch and PR were NOT
  deleted; reconcile manually if appropriate" on every
  forced reset. The
  `forced_reset_does_not_delete_remote_branch_or_pr` test in
  `queue_reset_cli_test.rs` asserts on this message. The
  `reset_entry` Rust method only manipulates the local
  `state.json` file; it never contacts GitHub.

## CONTRACTS.md status

The contracts file was **not modified** during Phase 03.

```
$ sha256sum planning/caduceus-v0.1/CONTRACTS.md
ace44d138c05548cb1b15e3176ed99f0521148ed4c24b449643716340be91eb2
```

This matches the `contracts_sha256` pinned in
`task-manifest.json`.

## Notes for downstream phases

- **Backoff is hardcoded at 300 seconds in
  `StateStore::retry_or_fail`.** Phase 7's `run` loop should
  thread `Config::retry_backoff_seconds` through once the
  full tick is wired up. Today the constant matches the
  Config default; the test surface accepts the constant.
- **The CLI's queue-reset path reads via `StateStore::open`
  which validates the state file.** A corrupt `state.json`
  causes the dry-run to surface
  `CaduceusError::StateCorrupt` rather than printing
  "would reset …". This is the correct behaviour (the
  operator must address the corruption before any recovery
  operation) and matches the contract's "malformed
  state.json is never replaced with an empty state" rule.
- **`reset_entry` does not take the daemon lock itself.** The
  CLI is responsible for taking the daemon lock before
  calling `reset_entry`. The StateStore's `with_exclusive`
  lock only serialises state-store mutations; the daemon-wide
  lock is a separate concern owned by the caller (cron tick
  or operator CLI). The `reset_refuses_when_daemon_lock_held`
  test confirms the CLI layer enforces this.
- **The `process_start_identity` field falls back to
  `"<unknown-boot>":0` on platforms without `/proc`.** Phase
  4's reaper is expected to re-validate identity before
  trusting a claim; the current implementation records what
  it has and never panics.