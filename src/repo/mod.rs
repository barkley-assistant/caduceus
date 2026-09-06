//! Daemon-owned repositories: bare mirrors and disposable worktrees.
//!
//! Every git subprocess created here goes through the hardened `GitRunner`.

pub mod mirror;
pub mod review_integrity;
pub mod review_worktree;
pub mod storage;
pub mod worktree;

pub use mirror::BareMirror;
pub use review_integrity::{
    capture_control_file_digests, check_tracked_files_clean, emit_review_mutation_violation,
    enforce_review_read_only, finish_mutation_violation, verify_control_file_digests,
    ReviewControlFileDigests, MUTATION_VIOLATION_EVENT, MUTATION_VIOLATION_SOURCE,
};
pub use review_worktree::{ReviewWorktree, ReviewWorktreeMetadata};
pub use storage::Storage;
