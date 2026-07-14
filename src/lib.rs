//! Caduceus v0.1
//!
//! Unix single-host, one-shot Rust daemon. This crate ships the `caduceus`
//! binary plus its library surface. Modules expose their canonical paths;
//! only a tightly bounded set of types is re-exported here so downstream
//! consumers and integration tests can reach them without depending on the
//! internal module layout.
//!
//! See [`CONTRACTS.md`] (under `planning/caduceus-v0.1/`) for the normative
//! scope of every module in this crate.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod config;
pub mod context;
pub mod error;
pub mod finalize;
pub mod github;
pub mod issue;
pub mod logging;
pub mod meta;
pub mod migrate;
pub mod poll;
pub mod prompt;
pub mod queue;
pub mod status;
pub mod validate;
pub mod verify;
pub mod worker;
pub mod worker_supervisor;
pub mod worktree;

// Bounded re-exports per CONTRACTS.md and Task 0.1's owning list.
// Modules remain reachable through their canonical paths for everything
// not listed here.
pub use crate::error::{CaduceusError, CaduceusResult};
pub use crate::issue::{IssueDetail, IssueKey};
pub use crate::queue::{
    ClaimToken, ClaimedEntry, DaemonLock, EnqueueOutcome, FinalizationCheckpoint,
    FinalizationStage, Phase, QueueEntry, QueueState, ResetOutcome, StateStore, TicketType,
};
pub use crate::worker::WorkerResult;
pub use crate::worktree::{
    find_main_clone, GitOutput, GitRunner, RepositoryInfo, WorktreeHandle, GIT_OUTPUT_BYTE_CAP,
};
