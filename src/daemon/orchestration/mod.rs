//! Orchestration layer for the per-tick pipeline.
//!
//! This module owns the canonical types the per-tick controller
//! passes between phases:
//!
//! * [`Services`] — the bundle of dependency-injection traits
//!   (`clock`, `github`, `git`, `executor`) used only where deterministic
//!   testing requires them. Production adapters are thin wrappers
//!   around the concrete types owned by [`crate::infra::config`],
//!   [`crate::github`], [`crate::worktree`], and
//!   [`crate::worker::supervisor`].
//! * [`FailureClass`] and [`classify_error`] — the exhaustive
//!   mapping from a [`CaduceusError`] to the four failure classes
//!   the orchestrator cares about (`Worker`, `Infrastructure`,
//!   `RateLimit`, `Cancellation`).
//! * [`ActiveRunGuard`] — the async cleanup primitive the
//!   orchestrator constructs after a successful claim. Its
//!   `finish_*` methods perform explicit state transitions;
//!   `Drop` only logs an invariant violation, never silently
//!   completes a transition.
//!
//! The shape of the canonical worker entry point
//! ([`crate::worker::supervisor::supervise`]), the canonical
//! worktree handle ([`crate::worktree::Worktree`]), and the
//! canonical finalization context ([`crate::finalize::FinalizeContext`])
//! are owned by their respective modules; this module's
//! `ActiveRunGuard` calls them by reference and never duplicates
//! their signatures.

// Submodule declarations and re-exports. These preserve the historical
// `crate::daemon::orchestration` public surface used by `lib.rs`.

pub mod active_run;
pub mod failure_class;
pub mod services;

use self::active_run::*;
use self::failure_class::*;
use self::services::*;

pub use active_run::*;
pub use failure_class::*;
pub use services::*;
