//! Per-run worktree management plus the shared git runner.
//!
//! Phase 4 owns the bodies for `GitRunner`, `RepositoryInfo`, the
//! `find_main_clone` discovery path, the daemon-owned worktree
//! `create` and `destroy` operations, and the GC sweep. The runner
//! is the single entry point for every git subprocess the daemon
//! spawns; it enforces the prompts/timeout/process-group contract
//! the rest of the crate relies on.

#![allow(clippy::module_inception)]

// Submodule declarations and re-exports. These preserve the historical
// `crate::worktree` public surface used by `lib.rs` and sibling modules.

pub mod gc;
pub mod git_runner;
pub mod repository;
pub mod testing;
pub mod worktree;

pub use gc::*;
pub use git_runner::*;
pub use repository::*;
pub use worktree::*;

pub use crate::worktree::{
    create as create_worktree, find_main_clone, remove as remove_worktree, GitOutput, GitRunner,
    RepositoryInfo, Worktree, GIT_OUTPUT_BYTE_CAP,
};
