//! Daemon-owned repositories: bare mirrors and disposable worktrees.
//!
//! Phase 5 owns this module. Every git subprocess created here goes
//! through the hardened GitRunner (Task 2.6).

pub mod mirror;
pub mod storage;
pub mod worktree;

pub use mirror::BareMirror;
pub use storage::Storage;
