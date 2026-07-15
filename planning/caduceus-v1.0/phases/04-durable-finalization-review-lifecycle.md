# Phase 04: Durable finalization and review lifecycle

## Intent

Make finalization crash-safe and preserve human review through PR merge.

## Tasks

- [Task 4.1: Persist finalization checkpoints][task-4-1]
- [Task 4.2: Reconcile ambiguous external side effects][task-4-2]
- [Task 4.3: Add human review lifecycle][task-4-3]
- [Task 4.4: Verify checkpoint and lifecycle recovery][task-4-4]

[task-4-1]: ../tasks/4.1-persist-finalization-checkpoints.md
[task-4-2]: ../tasks/4.2-reconcile-ambiguous-external-side-effects.md
[task-4-3]: ../tasks/4.3-add-human-review-lifecycle.md
[task-4-4]: ../tasks/4.4-verify-checkpoint-and-lifecycle-recovery.md

## Phase gate

- **PHASE-04-AC-01** — Checkpoint crash tests create no duplicates.
- **PHASE-04-AC-02** — Merge and unmerged-close states match the contract.

The gate handoff must map each phase acceptance ID to the exact command or
procedure, observed result, and durable artifact. The next phase does not
start until this gate is complete.
