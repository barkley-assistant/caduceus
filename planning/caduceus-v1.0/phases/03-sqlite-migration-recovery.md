# Phase 03: SQLite migration and recovery

## Intent

Introduce durable SQLite state with safe migration and supported recovery.

## Tasks

- [Task 3.1: Consolidate atomic file installation][task-3-1]
- [Task 3.2: Introduce versioned SQLite state store][task-3-2]
- [Task 3.3: Implement safe JSON-to-SQLite migration][task-3-3]
- [Task 3.4: Add supported state and metadata recovery][task-3-4]
- [Task 3.5: Model issue generations and reprocessing][task-3-5]
- [Task 3.6: Implement backup retention and state compaction][task-3-6]
- [Task 3.7: Add configuration schema v2][task-3-7]

[task-3-1]: ../tasks/3.1-consolidate-atomic-file-installation.md
[task-3-2]: ../tasks/3.2-introduce-versioned-sqlite-state-store.md
[task-3-3]: ../tasks/3.3-implement-safe-json-to-sqlite-migration.md
[task-3-4]: ../tasks/3.4-add-supported-state-and-metadata-recovery.md
[task-3-5]: ../tasks/3.5-model-issue-generations-and-reprocessing.md
[task-3-6]: ../tasks/3.6-implement-backup-retention-and-state-compaction.md
[task-3-7]: ../tasks/3.7-add-configuration-schema-v2.md

## Phase gate

- **PHASE-03-AC-01** — Migration rollback leaves source state unchanged.
- **PHASE-03-AC-02** — Recovery review is approved.
- **PHASE-03-AC-03** — Generations, retention, and compaction pass.

The gate handoff must map each phase acceptance ID to the exact command or
procedure, observed result, and durable artifact. The next phase does not
start until this gate is complete.
