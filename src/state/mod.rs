//! Durability surface — every persistent backend the daemon touches.
//!
//! Owns the JSON queue state, the JSON metadata file, the JSON → SQLite
//! migrators, the SQLite state store, and backup retention that bridges
//! both file formats.
//!
//! Module shape:
//!
//! - [`queue`] — the queue model, claim tokens, and the JSON StateStore
//!   locked with `flock`.
//! - [`meta`] — `MetaStore`, `CadenceGate`, and the rate-limit observer.
//! - [`store`] — the v1.0 SQLite StateStore.
//! - [`migrate`] — `caduceus migrate-state` (JSON import).
//! - [`migrate_to_sqlite`] — `caduceus migrate-state --to-sqlite`.
//! - [`retention`] — backup, corruption-archive, and finished-run
//!   artifact pruning, driven by `run_retention_days` on the tick
//!   cadence (issues #402, #403).

pub mod checkpoints;
pub mod meta;
pub mod migrate;
pub mod migrate_to_sqlite;
pub mod oci_run;
pub mod queue;
pub mod retention;
pub mod review;
pub mod store;

// Explicit re-exports of the queue's canonical surface — these are
// the types that downstream code reaches for when manipulating
// durable queue state.
pub use crate::state::queue::{
    parse_queue_state, serialize_queue_state, ClaimFileBody, ClaimToken, ClaimedEntry, DaemonLock,
    EnqueueOutcome, FinalizationCheckpoint, FinalizationStage, Phase, QueueEntry, QueueState,
    ResetOutcome, StateStore, TicketType,
};

// Explicit re-exports of the review stores' canonical surface (#295).
pub use crate::state::review::{
    parse_review_history, parse_review_queue_state, parse_review_state_map, review_claim_digest,
    review_queue_key, review_state_key, serialize_review_history, serialize_review_queue_state,
    serialize_review_state_map, ClaimedReview, ReviewClaimToken, ReviewEnqueueOutcome,
    ReviewHistoryFile, ReviewHistoryRow, ReviewPhase, ReviewQueueEntry, ReviewQueueState,
    ReviewStateMap, ReviewStore,
};

pub use crate::state::checkpoints::{
    checkpoint_for_run, delete_checkpoint, delete_checkpoints_for_run, last_checkpoint_for_run,
    persist_checkpoint, CheckpointRow,
};
