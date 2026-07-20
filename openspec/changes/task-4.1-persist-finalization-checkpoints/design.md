# Design: Task 4.1 — Persist finalization checkpoints

## Architecture Overview

The infrastructure (SQLite `checkpoints` table, CRUD functions in `checkpoints.rs`, integration tests) already exists. This task closes the gap between the contract and the runtime by: (1) extending `FinalizationStage` and `FinalizeAction` enums with the three missing FINAL-001 stages, (2) instrumenting `run_code_finalize()` with `persist_checkpoint()` calls before each external effect, (3) replacing the `Idle304` resume stub with a real checkpoint lookup, and (4) re-exporting the `checkpoints` module.

The key architectural insight: **checkpoint before effect, not after**. A crash between checkpoint write and effect is safe because the effect is idempotent; a crash *before* the checkpoint write is safe because the previous checkpoint is the resume point.

## Module Changes

### `src/state/queue.rs` — `FinalizationStage` enum

Current variants: `Committed`, `Pushed`, `PrCreated`, `Commented`, `InvestigationReady`, `InvestigationCommented`.

Add: `ResultValidated`, `AwaitingReview`, `Done`.

The existing `Investigation` variants are kept as-is (out of scope for removal; a future cleanup task will handle them).

Implement `as_str(&self) -> &'static str` — maps each variant to its `snake_case` string, matching the serde rename convention already used by the `#[serde(rename_all = "snake_case")]` attribute:

```rust
impl FinalizationStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ResultValidated => "result_validated",
            Self::Committed => "committed",
            Self::Pushed => "pushed",
            Self::PrCreated => "pr_created",
            Self::Commented => "commented",
            Self::AwaitingReview => "awaiting_review",
            Self::Done => "done",
            Self::InvestigationReady => "investigation_ready",
            Self::InvestigationCommented => "investigation_commented",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "result_validated" => Self::ResultValidated,
            "committed" => Self::Committed,
            "pushed" => Self::Pushed,
            "pr_created" => Self::PrCreated,
            "commented" => Self::Commented,
            "awaiting_review" => Self::AwaitingReview,
            "done" => Self::Done,
            "investigation_ready" => Self::InvestigationReady,
            "investigation_commented" => Self::InvestigationCommented,
            _ => return None,
        })
    }
}
```

These methods are required by `checkpoints.rs` — `CheckpointRow::stage_enum()` already calls `FinalizationStage::from_str()`, and `persist_checkpoint()` calls `stage.as_str()`.

### `src/finalize/mod.rs` — `FinalizeAction` enum

Current variants: `Committed`, `Pushed`, `PrCreated`, `Commented`, `Closed`, `InvestigationReady`, `InvestigationCommented`, `Previewed`.

Add: `ResultValidated`, `AwaitingReview`, `Done`.

Keep `InvestigationReady` / `InvestigationCommented` / `Previewed` / `Closed` as-is. The `FinalizeAction` enum mirrors `FinalizationStage` for type safety in `FinalizeOutput`, but uses `Closed` instead of `Done` (the PR close is a distinct action from the `Done` terminal stage).

The `FinalizeOutput` in `run_code_finalize()` currently hardcodes `action: FinalizeAction::Commented`. After this change, the action will reflect the actual stage — but note that `run_code_finalize()` is the orchestrator's sequence, and `FinalizeOutput` is returned from each step. The action comes from the individual step functions (`commit_code_and_finalize` returns `FinalizeAction::Committed`, etc.), so the new variants enable the checkpoint to carry the correct stage.

### `src/state/mod.rs` — Re-export

Add `pub mod checkpoints;` and re-export key types:

```rust
pub mod checkpoints;

pub use crate::state::checkpoints::{
    checkpoint_for_run, delete_checkpoint, delete_checkpoints_for_run,
    last_checkpoint_for_run, persist_checkpoint, CheckpointRow,
};
```

### `src/daemon/tick.rs` — Checkpoint writes in `run_code_finalize()`

The current `run_code_finalize()` calls four steps in sequence:

```rust
commit_code_and_finalize(ctx, worker_result, runner, worker_result_path)?;
push_and_finalize(ctx, runner).await?;
find_or_create_pr_and_finalize(ctx, client, worker_result).await?;
post_completion_and_close_and_finalize(ctx, client, worker_result).await?;
```

Each step is preceded by a `persist_checkpoint()` call. The function needs access to the SQLite `Connection` (from the state store) and the `run_id` (from `ctx.run_id`).

The instrumented sequence:

```rust
async fn run_code_finalize(
    ctx: &FinalizeContext,
    worker_result: &WorkerResult,
    runner: &GitRunner,
    worker_result_path: &std::path::Path,
    client: &Client,
    conn: &rusqlite::Connection,
) -> CaduceusResult<FinalizeOutput> {
    // Stage 1: ResultValidated — result has been validated, about to commit
    persist_checkpoint(conn, &ctx.run_id, FinalizationStage::ResultValidated, None)?;
    let commit_out = commit_code_and_finalize(ctx, worker_result, runner, worker_result_path)?;

    // Stage 2: Committed — commit is done, about to push
    persist_checkpoint(conn, &ctx.run_id, FinalizationStage::Committed, None)?;
    let push_out = push_and_finalize(ctx, runner).await?;

    // Stage 3: Pushed — push is done, about to create PR
    persist_checkpoint(conn, &ctx.run_id, FinalizationStage::Pushed, None)?;
    let pr_out = find_or_create_pr_and_finalize(ctx, client, worker_result).await?;

    // Stage 4: PrCreated — PR exists, about to post comment / close
    persist_checkpoint(conn, &ctx.run_id, FinalizationStage::PrCreated, None)?;
    let close_out = post_completion_and_close_and_finalize(ctx, client, worker_result).await?;

    // Stage 5: Commented — comment posted / issue closed
    persist_checkpoint(conn, &ctx.run_id, FinalizationStage::Commented, None)?;

    // Stage 6: AwaitingReview — waiting for human merge
    persist_checkpoint(conn, &ctx.run_id, FinalizationStage::AwaitingReview, None)?;

    // Stage 7: Done — finalization complete
    persist_checkpoint(conn, &ctx.run_id, FinalizationStage::Done, None)?;

    // Clean up checkpoints
    delete_checkpoints_for_run(conn, &ctx.run_id)?;

    Ok(FinalizeOutput {
        action: FinalizeAction::Done,
        pr_url: pr_out.pr_url,
        idempotency_observations: Vec::new(),
    })
}
```

The `conn` parameter is obtained from the `StateStore` (or from `store::open_in(&state_dir)`). The tick controller already opens the `StateStore` at line 185; the SQLite connection is available through the store's internal path. The cleanest approach: pass the `Connection` through `FinalizeContext` or open a fresh connection in `run_code_finalize()` via `store::open_in(&cfg.state_dir)`.

### `src/daemon/tick.rs` — Resume logic

Replace the `Idle304` stub at line 324 with real checkpoint lookup:

```rust
// 7a. If the entry already has a finalization checkpoint,
//     jump to the resume stage.
if claimed.entry.finalization.is_some() {
    let conn = crate::state::store::open_in(&cfg.state_dir)?;
    let run_id = claimed.entry.last_run_id.as_deref()
        .unwrap_or(guard.run_id());
    match resume_from_checkpoint(&conn, run_id)? {
        ResumeAction::Skip(stage) => {
            // Re-enter the finalization pipeline at `stage`
            return run_resume_finalization(cfg, services, store, meta, client, claimed, guard, cancellation, http_status, stage).await;
        }
        ResumeAction::AlreadyDone => {
            return Ok(TickOutcome::Processed);
        }
        ResumeAction::StartFresh => {
            // Fall through to normal flow
        }
    }
}
```

The `run_resume_finalization` function is a new helper that jumps into the finalization pipeline at the correct stage, skipping earlier stages. For the first cut, this is a separate function that mirrors `run_claim`'s finalization section but with a match on the resume stage.

## Resume Logic Pseudocode

```
enum ResumeAction {
    Skip(FinalizationStage),  // resume from this stage
    AlreadyDone,              // all stages complete
    StartFresh,              // no checkpoint, start from beginning
}

fn resume_from_checkpoint(conn, run_id) -> CaduceusResult<ResumeAction> {
    match last_checkpoint_for_run(conn, run_id)? {
        None => Ok(ResumeAction::StartFresh),
        Some(cp) => {
            let stage = match cp.stage_enum() {
                Some(s) => s,
                None => return Ok(ResumeAction::StartFresh),
            };
            match stage {
                FinalizationStage::Done => Ok(ResumeAction::AlreadyDone),
                other => Ok(ResumeAction::Skip(next_stage_after(other))),
            }
        }
    }
}

fn next_stage_after(stage: FinalizationStage) -> FinalizationStage {
    match stage {
        ResultValidated => Committed,
        Committed => Pushed,
        Pushed => PrCreated,
        PrCreated => Commented,
        Commented => AwaitingReview,
        AwaitingReview => Done,
        // Investigation stages pass through unchanged
        InvestigationReady => InvestigationCommented,
        InvestigationCommented => Done,
        Done => Done,
    }
}
```

## Sequence Diagram (Text)

```
run_code_finalize() flow:

  [Result Validated] --persist_checkpoint()--> SQLite
  [git commit]        --external effect-------> Local repo
  [Committed]         --persist_checkpoint()--> SQLite
  [git push]          --external effect-------> Remote repo
  [Pushed]            --persist_checkpoint()--> SQLite
  [find or create PR] --external effect-------> GitHub API
  [PrCreated]         --persist_checkpoint()--> SQLite
  [post comment/close]--external effect-------> GitHub API
  [Commented]         --persist_checkpoint()--> SQLite
  [AwaitingReview]    --persist_checkpoint()--> SQLite
  [Done]              --persist_checkpoint()--> SQLite
  [delete_checkpoints]--cleanup---------------> SQLite

Resume flow (daemon restart mid-sequence):

  [last_checkpoint_for_run] --reads MAX(created_at)--> SQLite
  [stage=Committed]         --next_stage_after()------> Pushed
  [skip to push step]       --enter run_code_finalize at Pushed-->
```

## Key Design Decisions

1. **INSERT OR REPLACE for idempotency** — The checkpoints table uses `INSERT OR REPLACE` so re-running a stage overwrites the previous checkpoint. This is already in the schema (`PRIMARY KEY (run_id, stage)`).

2. **Opaque JSON checkpoint_data** — The `checkpoint_data` field carries operation-specific markers (commit OID, PR URL) as opaque JSON. No typed schema until needed. The first cut passes `None` for all checkpoints; the data field is available for later phases.

3. **Checkpoint before effect, not after** — We persist the checkpoint BEFORE the external effect. This means a crash between checkpoint write and effect will re-execute the effect (which is idempotent), but guarantees the checkpoint is always present if the effect succeeds. If we wrote after the effect, a crash during the write would lose the checkpoint despite the effect having completed.

4. **Resume from last checkpoint, not first** — Reading the single most recent checkpoint is simpler and sufficient: the state machine is linear, so knowing the last completed stage tells us exactly where to resume. The `last_checkpoint_for_run` query already exists and uses `ORDER BY created_at DESC LIMIT 1`.

5. **Clean up on success** — `delete_checkpoints_for_run()` is called after the `Done` stage is persisted, keeping the table lean. If the daemon crashes before cleanup, the resume logic sees `Done` and returns `AlreadyDone` — the cleanup is lossless.

6. **Connection passed through FinalizeContext or opened fresh** — The SQLite `Connection` is a `!Send` handle, so it can't live in the async `FinalizeContext` directly. The cleanest approach is to open a fresh connection inside `run_code_finalize()` via `store::open_in(&cfg.state_dir)`, which is cheap since WAL mode allows concurrent readers.

## Integration Points

- **`checkpoints.rs` → `store.rs`**: The CRUD functions already use `Connection` (rusqlite), which is opened via `store::open()`.
- **`tick.rs` → `checkpoints.rs`**: The tick controller opens a SQLite connection and passes it to `run_code_finalize()`.
- **`FinalizeAction` → `FinalizationStage`**: The two enums are duals; `FinalizeAction` is used in `FinalizeOutput` returned by individual step functions, while `FinalizationStage` is used for checkpoint persistence. The `FinalizeOutput` from `run_code_finalize()` maps to `FinalizeAction::Done`, and the checkpoint stage is `FinalizationStage::Done`.
- **`run_claim()` → resume path**: The `Idle304` stub at line 324 is replaced with a checkpoint lookup that either starts fresh, skips to a stage, or returns `Processed` for already-complete runs.