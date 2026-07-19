# Phase gate handoff — Phase 03: SQLite migration and recovery

## Summary

All 7 tasks in Phase 03 are complete. The plan is valid (42 tasks, 8 phases,
acyclic and phase-safe). All 3 phase acceptance IDs are satisfied.

## Task handoffs

| Task | Title | Handoff |
|---|---|---|
| 3.1 | Consolidate atomic file installation | [`handoffs/3.1.md`](handoffs/3.1.md) |
| 3.2 | Introduce versioned SQLite state store | [`handoffs/3.2.md`](handoffs/3.2.md) |
| 3.3 | Implement safe JSON-to-SQLite migration | [`handoffs/3.3.md`](handoffs/3.3.md) |
| 3.4 | Add supported state and metadata recovery | [`handoffs/3.4.md`](handoffs/3.4.md) |
| 3.5 | Model issue generations and reprocessing | [`handoffs/3.5.md`](handoffs/3.5.md) |
| 3.6 | Implement backup retention and state compaction | [`handoffs/3.6.md`](handoffs/3.6.md) |
| 3.7 | Add configuration schema v2 | [`handoffs/3.7.md`](handoffs/3.7.md) |

All task handoffs are committed on `main` with their progress.json transitions.

## Phase gate acceptance evidence

| Acceptance ID | Status | Command or procedure | Result | Artifact |
|---|---|---|---|---|
| PHASE-03-AC-01 | PASS | `cargo test --locked -p caduceus --lib -- migrate_to_sqlite` | Migration rollback leaves source unchanged (tx rollback, dry-run) | tests/migrate_to_sqlite.rs |
| PHASE-03-AC-02 | PASS | Review artifact `handoffs/3.4-human-review.md` | Reviewed commit 9a7d1b179d7a0d451a41c8f0093a1ba262f84f51; decision: Approved | handoffs/3.4-human-review.md |
| PHASE-03-AC-03 | PASS | `cargo test --locked -p caduceus --lib -- retention` (4/4) + `cargo test -p caduceus --lib -- migrate_to_sqlite` (5/5) | Retention, generations, and compaction pass | tests/retention.rs, migrate_to_sqlite.rs |

## Verification commands

```
# Plan integrity
python3 -B planning/caduceus-v1.0/tools/validate_plan.py
→ plan valid (active catalog): 42 tasks, 8 phases, acyclic and phase-safe

# Full Rust test suite
cargo test --locked --all-targets
→ All tests pass

# Lints
cargo fmt --check         → clean
```

## CONTRACTS.md status

Sealed. `contracts_sha256` unchanged. No contract edits were required during
Phase 03 execution.

## Phase gate commits

| Commit | Description |
|---|---|
| 5432f4c | `feat(install): add atomic file write primitive for migration and recovery` |
| e54f480 | `feat(store): add versioned SQLite state store with schema v1` |
| 9ff3f08 | `feat(config): add worker_parallelism to configuration schema v2` |
| e2eac87 | `feat(migrate): add JSON-to-SQLite migration with --to-sqlite flag` |
| e8ec94c | `feat(queue): add generation tracking and reprocess command` |
| 9a7d1b1 | `feat(recovery): add SQLite state recovery with integrity check` |
| a2e0d3b | `feat(retention): add backup retention and state compaction` |

## New modules introduced

- `src/install.rs` — atomic write primitive (shared by migration, recovery, metadata)
- `src/store.rs` — versioned SQLite state store with schema v1
- `src/migrate_to_sqlite.rs` — JSON-to-SQLite migration command
- `src/retention.rs` — backup retention and state compaction

## Human review gates

| Task | Reviewer | Commit | Decision |
|---|---|---|---|
| 3.4 | JP-User | 9a7d1b179d7a0d451a41c8f0093a1ba262f84f51 | Approved |

## Residual risks

- The `caduceus migrate-state --to-sqlite` flag uses a hyphen (`--to-sqlite`)
  rather than the v0.1 documentation's `--to sqlite` (space-separated). The
  actual command is `caduceus migrate-state --to-sqlite`.
- Existing JSON state files without `generation` field will fail to parse
  until migrated. Migration from Task 3.3 sets generation=1.
- The production-surface scanner allowlist (from Phase 02) still has 2 entries.
  These should be driven to zero.

## Handoff artifacts

- [Phase 03 spec](../phases/03-sqlite-migration-recovery.md)
- [Phase gate handoff](phase-03.md) (this file)
- [CONTRACTS.md](../CONTRACTS.md)