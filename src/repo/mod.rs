//! Daemon-owned repositories: bare mirrors and disposable worktrees.
//!
//! Every git subprocess created here goes through the hardened `GitRunner`.

pub mod mirror;
pub mod review_worktree;
pub mod storage;
pub mod worktree;

pub use mirror::BareMirror;
pub use review_worktree::{ReviewWorktree, ReviewWorktreeMetadata};
pub use storage::Storage;
