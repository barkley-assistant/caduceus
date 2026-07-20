//! Caduceus v0.1
//!
//! Unix single-host, one-shot Rust daemon. This crate ships the `caduceus`
//! binary plus its library surface. Modules expose their canonical paths;
//! only a tightly bounded set of types is re-exported here so downstream
//! consumers and integration tests can reach them without depending on the
//! internal module layout.
//!
//! After the `src/` restructure (issue #13), the crate is organised into
//! eight subdirectories that match the boundaries `docs/architecture.md`
//! describes: `github/`, `worker/`, `state/`, `daemon/`, `worktree/`,
//! `finalize/`, `cli/`, `infra/`. Every public path that existed before
//! the restructure remains reachable through the `pub use` aliases below
//! — the integration test suite depends on the flat surface and we don't
//! force a coordinated rename of the test tree.
//!
//! See [`CONTRACTS.md`] (under `planning/caduceus-v0.1/`) for the normative
//! scope of every module in this crate.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

// --------------------------------------------------------------------------
// Module declarations. These point at the post-restructure subdirectories.
// --------------------------------------------------------------------------

pub mod daemon;
pub mod finalize;
pub mod github;
pub mod infra;
pub mod state;
pub mod worker;
pub mod worktree;

// --------------------------------------------------------------------------
// Flat `pub use` aliases.
//
// Goal: keep every external `caduceus::<old_module>::<thing>` import working
// unchanged. The integration tests under `tests/` reach into these names
// without going through `crate::`, so the surface here has to mirror what
// the pre-restructure `lib.rs` exposed.
// --------------------------------------------------------------------------

// Infra re-exports ---------------------------------------------------------
pub use crate::infra::config;
pub use crate::infra::error;
pub use crate::infra::fixtures;
pub use crate::infra::install;
pub use crate::infra::logging;
pub use crate::infra::validate;

// GitHub re-exports --------------------------------------------------------
pub use crate::github::client;
pub use crate::github::issue;
pub use crate::github::poll;
pub use crate::github::verify;

// Worker re-exports --------------------------------------------------------
pub use crate::worker::context;
pub use crate::worker::prompt;
pub use crate::worker::supervisor;
pub use crate::worker::supervisor as worker_supervisor;
// `crate::worker::` itself is the multi-file dir; `crate::worker_supervisor`
// was the pre-restructure name for what is now `crate::worker::supervisor`.

// State re-exports ---------------------------------------------------------
pub use crate::state::meta;
pub use crate::state::migrate;
pub use crate::state::migrate_to_sqlite;
pub use crate::state::queue;
pub use crate::state::retention;
pub use crate::state::store;

// Daemon re-exports --------------------------------------------------------
pub use crate::daemon::orchestration;
pub use crate::daemon::signals;
pub use crate::daemon::status;
pub use crate::daemon::tick;

// --------------------------------------------------------------------------
// Bounded symbol re-exports per CONTRACTS.md and Task 0.1's owning list.
//
// Modules remain reachable through their canonical paths for everything
// not listed here.
// --------------------------------------------------------------------------

pub use crate::daemon::orchestration::{
    ActiveRunGuard, Clock, FailureClass, FinishOutcome, Git, GithubClient, ProcessSupervisor,
    ProcessSupervisorAdapter, Services, SystemClock,
};
pub use crate::github::issue::{IssueDetail, IssueKey};
pub use crate::infra::error::{CaduceusError, CaduceusResult};
pub use crate::state::queue::{
    ClaimToken, ClaimedEntry, DaemonLock, EnqueueOutcome, FinalizationCheckpoint,
    FinalizationStage, Phase, QueueEntry, QueueState, ResetOutcome, StateStore, TicketType,
};
pub use crate::worker::WorkerResult;
pub use crate::worktree::{
    create as create_worktree, find_main_clone, remove as remove_worktree, GitOutput, GitRunner,
    RepositoryInfo, Worktree, GIT_OUTPUT_BYTE_CAP,
};
