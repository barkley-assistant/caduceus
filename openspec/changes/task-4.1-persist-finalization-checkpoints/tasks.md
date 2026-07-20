# Tasks: Task 4.1 — Persist finalization checkpoints

## Change Summary

- **Review workload forecast:** ~200 lines changed (4 files)
- **Chained PRs recommended:** No (single commit)
- **400-line budget risk:** Low
- **Decision needed before apply:** No

## Implementation Tasks

### T1: Extend `FinalizationStage` enum with three variants + serialization helpers

**Files:** `src/state/queue.rs`
**Test:** `tests/runtime/finalize_checkpoints_test.rs`
**Status:** [ ] pending

**Changes:**
- Add `ResultValidated`, `AwaitingReview`, `Done` variants to the `FinalizationStage` enum (keeping existing `Committed`, `Pushed`, `PrCreated`, `Commented`, `InvestigationReady`, `InvestigationCommented`)
- Implement `pub fn as_str(&self) -> &'static str` — maps each variant to its `snake_case` string:
  - `ResultValidated` → `"result_validated"`
  - `Committed` → `"committed"`
  - `Pushed` → `"pushed"`
  - `PrCreated` → `"pr_created"`
  - `Commented` → `"commented"`
  - `AwaitingReview` → `"awaiting_review"`
  - `Done` → `"done"`
  - `InvestigationReady` → `"investigation_ready"`
  - `InvestigationCommented` → `"investigation_commented"`
- Implement `pub fn from_str(s: &str) -> Option<Self>` — reverse of `as_str()`, returns `None` for unknown strings

**Why:** `checkpoints.rs` calls both `stage.as_str()` (in `persist_checkpoint()`) and `FinalizationStage::from_str()` (in `CheckpointRow::stage_enum()`). The existing unit/integration tests already reference these methods and the three new variants — the code won't compile without this task.

**Verification:** `cargo test --locked --all-targets persist_and_read_back` (unit test in `checkpoints.rs` exercises both methods indirectly).

---

### T2: Extend `FinalizeAction` enum with three variants

**Files:** `src/finalize/mod.rs`
**Test:** compile check only
**Status:** [ ] pending

**Changes:**
- Add `ResultValidated`, `AwaitingReview`, `Done` variants to the `FinalizeAction` enum
- Keep existing variants unchanged (`Committed`, `Pushed`, `PrCreated`, `Commented`, `Closed`, `InvestigationReady`, `InvestigationCommented`, `Previewed`)

**Why:** `FinalizeOutput.action` carries a `FinalizeAction` that mirrors the checkpoint `FinalizationStage`. The instrumented `run_code_finalize()` returns `FinalizeAction::Done` on completion, and the step functions may return `ResultValidated` / `AwaitingReview` from the checkpoint writes (though the individual step functions `commit_code_and_finalize`, `push_and_finalize`, etc. still return their own actions — the new variants enable the final output to carry the terminal stage).

**Verification:** `cargo check --locked --all-targets` compiles cleanly.

---

### T3: Re-export `checkpoints` module from `src/state/mod.rs`

**Files:** `src/state/mod.rs`
**Test:** compile check only
**Status:** [ ] pending

**Changes:**
- Add `pub mod checkpoints;` alongside the existing module declarations
- Re-export the public API surface:
  ```rust
  pub use crate::state::checkpoints::{
      checkpoint_for_run, delete_checkpoint, delete_checkpoints_for_run,
      last_checkpoint_for_run, persist_checkpoint, CheckpointRow,
  };
  ```

**Why:** The integration test `tests/runtime/finalize_checkpoints_test.rs` imports `caduceus::state::checkpoints::{...}`. Without this re-export the module is private and the test fails to compile. The re-export also makes the checkpoint API available to the daemon controller in `tick.rs`.

**Verification:** `cargo test --locked --all-targets finalize_checkpoints_test` compiles (tests may fail at runtime until T4/T5 are done, but imports resolve).

---

### T4: Instrument `run_code_finalize()` with checkpoint writes

**Files:** `src/daemon/tick.rs`
**Test:** `tests/runtime/finalize_checkpoints_test.rs` (AC-01, AC-02)
**Status:** [ ] pending

**Changes:**

1. **Open a SQLite connection inside `run_code_finalize()`** using `store::open_in(&ctx.config.state_dir)` — the `Connection` is `!Send` so it can't live in the async `FinalizeContext`; a fresh WAL-mode connection is cheap.

2. **Insert `persist_checkpoint()` calls before each external effect**, following the "checkpoint before effect" pattern:
   - Before `commit_code_and_finalize` → persist `FinalizationStage::ResultValidated`
   - Before `push_and_finalize` → persist `FinalizationStage::Committed`
   - Before `find_or_create_pr_and_finalize` → persist `FinalizationStage::Pushed`
   - Before `post_completion_and_close_and_finalize` → persist `FinalizationStage::PrCreated`

3. **After the four steps complete**, persist three terminal checkpoints in sequence:
   - `FinalizationStage::Commented` (comment/close posted)
   - `FinalizationStage::AwaitingReview` (waiting for human merge)
   - `FinalizationStage::Done` (finalization complete)

4. **Clean up** — call `delete_checkpoints_for_run(conn, &ctx.run_id)` after persisting `Done`.

5. **Update the return value** to `FinalizeAction::Done` instead of the hardcoded `FinalizeAction::Commented`.

6. **Return `CaduceusError::StateCorrupt`** if any checkpoint write fails (which triggers tick retry; the last durable checkpoint survives).

**Signature change:**
```rust
async fn run_code_finalize(
    ctx: &FinalizeContext,
    worker_result: &WorkerResult,
    runner: &GitRunner,
    worker_result_path: &std::path::Path,
    client: &Client,
) -> CaduceusResult<FinalizeOutput>
```
No signature change needed — `ctx.config.state_dir` gives access to the SQLite store path.

**Why:** The test `persist_all_seven_checkpoints` writes all seven stages directly and verifies they round-trip. Once `run_code_finalize()` writes checkpoints, the integration test that exercises the full finalization pipeline will produce all seven rows in order (AC-01, AC-02).

**Verification:** Run `cargo test --locked --all-targets` — the integration tests in `finalize_checkpoints_test.rs` should pass (they write directly, not through `run_code_finalize`). The AC-01/AC-02 tests verify the data-flow side; full pipeline tests that call `run_code_finalize()` are end-to-end and part of a later phase.

---

### T5: Replace resume stub with real checkpoint lookup

**Files:** `src/daemon/tick.rs`
**Test:** `tests/runtime/finalize_checkpoints_test.rs` (AC-03)
**Status:** [ ] pending

**Changes:**

1. **Replace the `Idle304` stub at line 324** (`run_claim` function) with real checkpoint lookup logic:
   ```rust
   if claimed.entry.finalization.is_some() {
       let conn = crate::state::store::open_in(&cfg.state_dir)?;
       let run_id = claimed.entry.last_run_id.as_deref()
           .unwrap_or_else(|| guard.run_id());
       match resume_from_checkpoint(&conn, run_id)? {
           ResumeAction::Skip(stage) => {
               return run_resume_finalization(
                   cfg, services, store, meta, client, claimed,
                   guard, cancellation, http_status, stage,
               ).await;
           }
           ResumeAction::AlreadyDone => {
               return Ok(TickOutcome::Processed);
           }
           ResumeAction::StartFresh => {
               // Fall through to normal flow below
           }
       }
   }
   ```

2. **Add `ResumeAction` enum** (private helper):
   ```rust
   enum ResumeAction {
       Skip(FinalizationStage),
       AlreadyDone,
       StartFresh,
   }
   ```

3. **Add `resume_from_checkpoint()` function** — queries `last_checkpoint_for_run(conn, run_id)` and returns the appropriate `ResumeAction`:
   - `None` → `StartFresh`
   - `Some(cp)` where `cp.stage_enum()` returns `Done` → `AlreadyDone`
   - `Some(cp)` where `cp.stage_enum()` returns a prior stage → `Skip(next_stage_after(stage))`

4. **Add `next_stage_after()` function** — maps each stage to the next in sequence:
   ```
   ResultValidated → Committed
   Committed → Pushed
   Pushed → PrCreated
   PrCreated → Commented
   Commented → AwaitingReview
   AwaitingReview → Done
   Done → Done
   InvestigationReady → InvestigationCommented
   InvestigationCommented → Done
   ```

5. **Add `run_resume_finalization()` helper** — a separate function that jumps into the finalization pipeline at the given stage, skipping earlier stages. Structure mirrors the normal `run_code_finalize()` flow but with a match that skips completed stages:
   ```
   match resume_stage {
       ResultValidated => { /* start from commit */ }
       Committed => { /* skip commit, start from push */ }
       Pushed => { /* skip commit+push, start from PR */ }
       PrCreated => { /* skip through PR, start from comment */ }
       Commented => { /* all done, persist terminal */ }
       AwaitingReview => { /* already at terminal */ }
       Done => { /* already at terminal */ }
       InvestigationReady | InvestigationCommented => { /* fall through */ }
   }
   ```
   The helper opens a SQLite connection to persist checkpoints, calls the remaining step functions, and cleans up checkpoints on completion — same pattern as `run_code_finalize()` but with a resume offset.

**Why:** AC-03 requires that after a simulated crash (checkpoints up to `Pushed`), the resume logic skips already-completed stages and returns the correct last checkpoint. The `resume_returns_last_checkpoint` and `resume_returns_none_for_unknown_run` tests verify this. The `Idle304` stub blocks all resume scenarios.

**Verification:** `cargo test --locked --all-targets` — `finalize_checkpoints_test.rs::resume_returns_last_checkpoint` and `resume_returns_none_for_unknown_run` must pass.

---

## Dependency Graph

```
T1 (queue.rs: variants + methods)
  └── required by T4 (checkpoint writes call as_str())
  └── required by T5 (resume calls stage_enum() → from_str())
  └── required by test imports (ALL_STAGES contains new variants)

T2 (finalize/mod.rs: variants)
  └── required by T4 (FinalizeOutput::action uses Done)

T3 (state/mod.rs: re-export)
  └── required by T4 (import persist_checkpoint in tick.rs)
  └── required by test imports (checkpoints module)

T4 (tick.rs: instrument run_code_finalize)
  └── depends on T1, T2, T3
  └── blocked until variants + re-export compile

T5 (tick.rs: resume logic)
  └── depends on T1, T3
  └── blocked until variants + re-export compile
  └── independent of T2, T4 (T5 adds new helpers, doesn't modify run_code_finalize)
```

**Recommended apply order:** T1 → T2 → T3 → T4 → T5 (topological). Each step compiles and can be verified independently.
