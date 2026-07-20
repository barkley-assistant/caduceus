# Proposal: Task 4.1 — Persist finalization checkpoints

## Intent

Make the finalization sequence crash-safe by persisting a checkpoint
before every external effect, and wire the resume path so a daemon
restart picks up where it left off instead of returning `Idle304`.

## Scope

### In scope

- Add `ResultValidated`, `AwaitingReview`, `Done` to
  `FinalizationStage` in `src/state/queue.rs`
- Add `as_str()` and `from_str()` to `FinalizationStage`
- Mirror the same three variants in `FinalizeAction` in
  `src/finalize/mod.rs` (+ remove `InvestigationReady` /
  `InvestigationCommented` if they are dead outside the contract
  sequence; or keep them and add the new ones alongside)
- Insert `persist_checkpoint()` calls into `run_code_finalize()`
  in `src/daemon/tick.rs` — one before each of the four steps
  (commit, push, PR, comment/close)
- Replace the `Idle304` resume stub at `tick.rs:324` with real
  logic that reads the last checkpoint from SQLite and skips
  already-completed stages
- Call `delete_checkpoints_for_run()` on successful completion
- Re-export `checkpoints` module from `src/state/mod.rs`

### Out of scope

- Typed checkpoint data schemas (stays opaque JSON for now)
- The `FinalizationStage::InvestigationReady` /
  `InvestigationCommented` variants — removing them is a
  refactor that touches unrelated code; leave them unused
  until a dedicated cleanup task
- `AwaitingReview` → `Done` transition via PR merge webhook
  (separate task in phase 4)
- End-to-end resume test in `tick.rs` (integration test exists
  in `finalize_checkpoints_test.rs`; full daemon resume is
  Task 7.5)

## Approach

The infrastructure (SQLite schema, CRUD functions, integration
tests) is already in place. This work fills the gap between the
contract and the runtime by:

1. **Extending the stage enums** so the code can represent all
   seven stages from FINAL-001, plus adding `as_str()` /
   `from_str()` serialization helpers that `checkpoints.rs`
   already calls.
2. **Instrumenting the finalization pipeline** so every external
   effect is preceded by a SQLite checkpoint write.
3. **Replacing the resume stub** with a real checkpoint lookup
   that skips already-completed stages.
4. **Cleaning up** checkpoints on completion and publicly
   exporting the module.

## Implementation Outline

1. **Extend `FinalizationStage`** (`src/state/queue.rs`)
   - Add `ResultValidated`, `AwaitingReview`, `Done` variants
   - Implement `as_str(&self) -> &'static str` mapping each
     variant to its snake_case name
   - Implement `from_str(s: &str) -> Option<Self>`
   - Keep existing variants unchanged

2. **Extend `FinalizeAction`** (`src/finalize/mod.rs`)
   - Add `ResultValidated`, `AwaitingReview`, `Done`
   - Keep `InvestigationReady` / `InvestigationCommented` /
     `Previewed` / `Closed` as-is (out of scope)

3. **Re-export `checkpoints` module** (`src/state/mod.rs`)
   - Add `pub mod checkpoints;` and `pub use
     crate::state::checkpoints::{...};` for the public API
     surface

4. **Instrument `run_code_finalize()`** (`src/daemon/tick.rs`)
   - Before each step in `run_code_finalize()`, call
     `persist_checkpoint()` with the corresponding stage:
     - Before `commit_code_and_finalize` → `ResultValidated`
     - Before `push_and_finalize` → `Committed`
     - Before `find_or_create_pr_and_finalize` → `Pushed`
     - Before `post_completion_and_close_and_finalize` →
       `PrCreated` (then `Commented` after the step)
   - After the final step, call `persist_checkpoint()` with
     `AwaitingReview` (or `Done` for auto-close)
   - On success, call `delete_checkpoints_for_run()`

5. **Wire resume logic** (`src/daemon/tick.rs` at line 324)
   - Replace the `Idle304` stub with:
     - Read `last_checkpoint_for_run(conn, run_id)`
     - If `None`, proceed normally (first run)
     - If `Some`, match the stage to skip completed steps
       and jump to the next uncompleted stage
   - Return `TickOutcome::Processed` if all stages are
     already done (idempotent re-run)

## Files Changed

- `src/state/queue.rs` — Add 3 enum variants, implement
  `as_str()` and `from_str()`
- `src/finalize/mod.rs` — Add 3 variants to `FinalizeAction`
- `src/state/mod.rs` — Re-export `checkpoints` module
- `src/daemon/tick.rs` — Insert checkpoint persistence calls
  in `run_code_finalize()`; replace resume stub with real
  checkpoint lookup

## Acceptance Criteria

- **4.1-AC-01** — Persist all seven checkpoints. Every
  FINAL-001 stage is written to the SQLite checkpoint table
  before the corresponding external effect. (Existing test
  `persist_all_seven_checkpoints` passes — needs the 3 new
  variants to compile)
- **4.1-AC-02** — Commit a checkpoint before its next effect.
  Each checkpoint row has a `created_at` that precedes the
  next checkpoint's `created_at`.
  (`checkpoints_are_chronologically_ordered` passes)
- **4.1-AC-03** — Resume from the last durable checkpoint.
  After a simulated crash, reading the checkpoint table
  returns the most recent stage so the orchestrator can
  resume. (`resume_returns_last_checkpoint` passes)

## Risks

1. **Enum variant mismatch** — `FinalizationStage` and
   `FinalizeAction` are dual enums that must stay in sync.
   Adding variants to one but forgetting the other will cause
   compile errors in `FinalizeOutput` construction. Mitigation:
   both are in the same diff, reviewed together.
2. **Resume logic is naive** — The current design jumps to the
   last checkpoint stage and re-executes. If the external
   system is in an inconsistent state (half-pushed branch,
   orphan PR), re-execution may fail. Mitigation: first cut
   just re-runs the stage; idempotency guards in the
   finalize helpers handle the common case. Full reconciliation
   is deferred to later phases.
3. **Checkpoint write fails mid-sequence** — A SQLite write
   failure between stages leaves the run in an inconsistent
   state. Mitigation: the error propagates as a
   `CaduceusError::StateCorrupt`, which causes the daemon to
   retry the entire tick; the last checkpoint was written
   before the failed effect, so the next retry resumes from
   the correct stage.

## Rollback Plan

Revert the commit. The `checkpoints` table and CRUD functions
exist in the schema but are unused before this change — no
data loss or migration needed. The `Idle304` stub remains as
the safe fallback for the resume path.