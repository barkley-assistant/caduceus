# Exploration Report — Task 4.1: Persist finalization checkpoints

**Date:** 2026-07-20
**Change:** task-4.1-persist-finalization-checkpoints
**Issue:** https://github.com/barkley-assistant/caduceus/issues/19

## 1. Executive Summary

The checkpoint persistence infrastructure is **already scaffolded** — the SQLite `checkpoints` table exists in the schema, the `src/state/checkpoints.rs` module provides CRUD operations, and an integration test suite at `tests/runtime/finalize_checkpoints_test.rs` already passes. What's missing: the `FinalizationStage` enum in `src/state/queue.rs` only has 6 stages and needs `ResultValidated`, `AwaitingReview`, and `Done` to match the 7-stage FINAL-001 contract. The resume-from-last-checkpoint logic in `src/daemon/tick.rs:324` currently returns `Idle304` as a stub — it needs to be wired to actually read checkpoints and resume.

## 2. Existing Architecture

| Module | Path | Purpose |
|--------|------|---------|
| `state/store.rs` | `src/state/store.rs` | SQLite schema v1, `open()` with WAL mode, version check, 6 tables |
| `state/checkpoints.rs` | `src/state/checkpoints.rs` | CRUD for `checkpoints` table — fully implemented |
| `state/queue.rs` | `src/state/queue.rs` | `FinalizationStage` enum (6 stages), `FinalizationCheckpoint` struct |
| `daemon/tick.rs` | `src/daemon/tick.rs` | Per-tick controller; `run_claim` has a stub at line 324 for checkpoint resume |
| `finalize/mod.rs` | `src/finalize/mod.rs` | `FinalizeContext`, `FinalizeAction`, `commit_code_result`, `push_daemon_branch`, etc. |
| `daemon/orchestration.rs` | `src/daemon/orchestration.rs` | `ActiveRunGuard`, `Services` DI bundle, `FailureClass` classification |

The crate is structured into 8 subdirectories under `src/`: `github/`, `worker/`, `state/`, `daemon/`, `worktree/`, `finalize/`, `cli/`, `infra/`.

## 3. State Store Details

**Schema (v1, in `src/state/store.rs:39-89`):**

```sql
CREATE TABLE schema_version (version INTEGER NOT NULL, migrated_at TEXT NOT NULL);
CREATE TABLE queue_entries (issue_key TEXT PRIMARY KEY, phase TEXT NOT NULL, ...);
CREATE TABLE state_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE claims (claim_id TEXT PRIMARY KEY, issue_key TEXT NOT NULL, ...);
CREATE TABLE checkpoints (
    run_id          TEXT NOT NULL,
    stage           TEXT NOT NULL,
    checkpoint_data TEXT,
    created_at      TEXT NOT NULL,
    PRIMARY KEY (run_id, stage)
);
CREATE TABLE circuit_breakers (...);
```

**Migration pattern:** `store::open()` checks `schema_version` table. Fresh DB → `init_schema()`. Existing DB with lower version → `apply_schema()` (idempotent, `CREATE TABLE IF NOT EXISTS`). Higher version → reject with `StateCorrupt`. WAL mode is enabled.

**Checkpoints CRUD** (`src/state/checkpoints.rs:71-186`):
- `persist_checkpoint(conn, run_id, stage, checkpoint_data)` — `INSERT OR REPLACE`
- `checkpoint_for_run(conn, run_id)` — all checkpoints ordered by `created_at ASC`
- `last_checkpoint_for_run(conn, run_id)` — most recent checkpoint
- `delete_checkpoints_for_run(conn, run_id)` — cleanup on completion
- `delete_checkpoint(conn, run_id, stage)` — specific stage deletion

**Current FinalizationStage enum** (`src/state/queue.rs:100-107`):
```rust
pub enum FinalizationStage {
    Committed,
    Pushed,
    PrCreated,
    Commented,
    InvestigationReady,
    InvestigationCommented,
}
```

Missing: `ResultValidated`, `AwaitingReview`, `Done`. The `as_str()` and `from_str()` methods are referenced in `checkpoints.rs` but don't exist yet.

## 4. Runtime Structure

**`src/daemon/tick.rs`** is the canonical tick controller:
1. Load config, init logging
2. Acquire `DaemonLock`
3. Open `MetaStore`, `CadenceGate`
4. Reap stale claims
5. Discover repos, poll GitHub, enqueue
6. `acquire_next()` — get next eligible entry
7. **Resume checkpoint check** (line 324): currently a **stub** returning `Idle304`
8. `run_claim()` — verify label, fetch issue, discover repo, create worktree, etc.
9. `run_code_finalize()` (line 563): commit → push → PR → comment/close

## 5. Main Entry Point

**`src/main.rs`** is minimal. It dispatches to `__worker-supervisor` mode or `cli::run()`. The CLI handles `run`, `status`, `worktree-gc`, `queue reset`, `migrate-state`, `setup`. No-arg invocation rewrites to `caduceus run`.

There is **no startup resume logic** currently — the daemon is one-shot per tick. The "resume on restart" semantics happen naturally because each tick reads the current queue state from SQLite. If a tick crashes mid-finalization, the next tick will find the entry and checkpoints table, then resume from there.

## 6. Test Patterns

**Checkpoint tests** (`tests/runtime/finalize_checkpoints_test.rs`, 222 lines, 7 tests):
- `persist_all_seven_checkpoints` — writes all 7 stages, reads back in order
- `checkpoints_are_chronologically_ordered` — verifies `created_at` ordering
- `resume_returns_last_checkpoint` — writes first 3 stages, verifies last is `Pushed`
- `resume_returns_none_for_unknown_run` — empty result for unknown run
- `persist_checkpoint_with_operation_data` — round-trips JSON checkpoint data
- `repersist_same_stage_overwrites` — `INSERT OR REPLACE` idempotency

**State store tests** (`tests/state_store_test.rs`, 788 lines): Tests for `save_finalization`, `set_worktree`, acquire, enqueue, complete, retry, concurrency.

## 7. Key Risks & Gotchas

1. **`FinalizationStage` enum mismatch**: Contract specifies 7 stages (`ResultValidated → Committed → Pushed → PrCreated → Commented → AwaitingReview → Done`), but the current enum has 6 stages missing `ResultValidated`, `AwaitingReview`, `Done`. Also has `InvestigationReady`/`InvestigationCommented` which aren't in the FINAL-001 sequence.
2. **Dual persistence paths**: Checkpoints table (new) vs. `save_finalization` on queue entries (old). Resume must use the `checkpoints` table for crash recovery.
3. **Resume stub**: `tick.rs:324` returns `Idle304` — must be replaced with actual checkpoint reading.
4. **`checkpoint_data` format**: Opaque JSON — no typed schema yet.
5. **No checkpoint deletion on completion**: `delete_checkpoints_for_run()` exists but isn't called.
6. **Checkpoint writes not yet inserted**: `persist_checkpoint()` calls need to be added to `run_code_finalize()`.
7. **`src/state/mod.rs`**: `checkpoints` module is not explicitly re-exported — works via direct import but could be cleaner.

## 8. Key References

| File | Lines | Purpose |
|------|-------|---------|
| `src/state/queue.rs` | 95-107 | `FinalizationStage` enum (needs 3 new variants) |
| `src/state/queue.rs` | 109-120 | `FinalizationCheckpoint` struct |
| `src/state/checkpoints.rs` | 1-286 | Full checkpoint CRUD module (already complete) |
| `src/state/store.rs` | 39-89 | SQLite schema with `checkpoints` table |
| `src/state/mod.rs` | 1-31 | Re-exports |
| `src/daemon/tick.rs` | 320-326 | Resume stub |
| `src/daemon/tick.rs` | 563-579 | `run_code_finalize()` — where checkpoint writes should go |
| `src/finalize/mod.rs` | 131-144 | `FinalizeAction` enum (also needs update) |
| `tests/runtime/finalize_checkpoints_test.rs` | 1-222 | Integration test suite (uses `ALL_STAGES` with all 7 stages) |
| `planning/caduceus-v1.0/CONTRACTS.md` | 366-383 | FINAL-001 contract |
| `planning/caduceus-v1.0/tasks/4.1-persist-finalization-checkpoints.md` | 1-41 | Task packet |