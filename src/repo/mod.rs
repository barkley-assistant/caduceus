//! Daemon-owned repositories: bare mirrors and disposable worktrees.
//!
//! Every git subprocess created here goes through the hardened `GitRunner`.

pub mod mirror;
pub mod storage;
pub mod worktree;

pub use mirror::BareMirror;
pub use storage::Storage;
