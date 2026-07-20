# Finalization Checkpoints Specification

## Purpose

Make the finalization sequence crash-safe by persisting a checkpoint
before each external effect, so a daemon restart resumes from the last
durable stage instead of losing progress. The checkpoint table acts as
the single source of truth for which FINAL-001 stages have completed.

## Requirements

### Requirement: Persist all seven checkpoints

The system MUST persist a checkpoint for each of the seven
FINAL-001 stages — `ResultValidated`, `Committed`, `Pushed`,
`PrCreated`, `Commented`, `AwaitingReview`, `Done` — to the SQLite
`checkpoints` table before the corresponding external effect executes.

#### Scenario: Happy path writes every stage

- GIVEN a run enters the finalization pipeline
- WHEN the system processes each FINAL-001 stage in sequence
- THEN a row MUST exist in the `checkpoints` table for every stage
  with the correct `run_id` and `stage` values

#### Scenario: Stage order is chronologically consistent

- GIVEN a run that completed all seven stages
- WHEN the `checkpoints` table is queried ordered by `created_at`
- THEN the stages MUST appear in the sequence `ResultValidated`,
  `Committed`, `Pushed`, `PrCreated`, `Commented`, `AwaitingReview`,
  `Done`

### Requirement: Commit checkpoint before its effect

The system MUST write a checkpoint row to the SQLite `checkpoints`
table before each external effect (git commit, git push, PR creation,
comment or issue close). The checkpoint MUST be durable before the
effect starts. The `checkpoints` table MUST use `INSERT OR REPLACE`
semantics so restarting a stage overwrites the previous checkpoint
for the same `(run_id, stage)` tuple.

#### Scenario: Checkpoint precedes its external effect

- GIVEN a run is about to execute a specific FINAL-001 stage
- WHEN the system calls `persist_checkpoint()` for that stage
- THEN the checkpoint MUST be committed to SQLite before the
  corresponding external effect begins execution

#### Scenario: Re-persisting the same stage is idempotent

- GIVEN a checkpoint already exists for `(run_id, stage)`
- WHEN the system persists the same stage again
- THEN the existing row MUST be replaced without error, and the
  `created_at` timestamp MUST be updated

### Requirement: Resume from last durable checkpoint

On daemon restart, the system MUST read the most recent checkpoint
for the run and resume from the corresponding stage. Stages whose
checkpoints already exist MUST be skipped. The resume path MUST NOT
re-execute external effects whose checkpoints are already committed.

#### Scenario: Resume after crash mid-sequence

- GIVEN a run has checkpoints for `ResultValidated`, `Committed`,
  and `Pushed`
- WHEN the daemon restarts and reads the last checkpoint
- THEN the system MUST resume from `PrCreated` and MUST NOT
  re-execute commit, push, or result validation

#### Scenario: Unknown run returns None

- GIVEN a run_id with no rows in the `checkpoints` table
- WHEN the system calls `last_checkpoint_for_run(conn, run_id)`
- THEN it MUST return `None`

#### Scenario: Crash between checkpoint write and external effect

- GIVEN a checkpoint was written for `Committed` but the git push
  did not complete before the crash
- WHEN the daemon restarts and reads the last checkpoint
- THEN the system MUST resume from `Committed` (the push will
  be retried; idempotency guards prevent duplicate effects)

#### Scenario: All stages already complete on resume

- GIVEN a run has a checkpoint for `Done`
- WHEN the daemon restarts
- THEN the system MUST skip all finalization stages and return
  `TickOutcome::Processed` without executing any external effects

### Requirement: Clean up checkpoints on success

The system MUST delete all checkpoints for a run upon successful
completion of the finalization sequence.

#### Scenario: Checkpoints removed after completion

- GIVEN a run completed the full FINAL-001 sequence
- WHEN `delete_checkpoints_for_run(conn, run_id)` is called
- THEN all rows for that `run_id` MUST be removed from the
  `checkpoints` table

#### Scenario: Checkpoints survive a crash before cleanup

- GIVEN a run completed the full sequence but crashed before
  `delete_checkpoints_for_run()` executed
- WHEN the daemon restarts
- THEN the checkpoints MUST still exist in the table, and the
  resume logic MUST detect all stages complete and return
  `TickOutcome::Processed`