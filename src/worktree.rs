//! Per-run worktree management. Phase 4 owns the bodies; the stub defines
//! the typed surface.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::CaduceusResult;
use crate::issue::IssueKey;

/// Outcome of creating one daemon-owned worktree + branch.
#[derive(Debug)]
pub struct WorktreeHandle {
    pub issue: IssueKey,
    pub run_id: String,
    pub branch_name: String,
    pub path: PathBuf,
    pub created_at: DateTime<Utc>,
}

/// Provision an isolated worktree + branch.
pub fn create(_key: &IssueKey, _run_id: &str) -> CaduceusResult<WorktreeHandle> {
    Err(crate::error::CaduceusError::Worktree {
        context: "create",
        stderr: "create() implementation lives in Task 4.2".to_string(),
    })
}

/// Tear down a worktree, refusing to remove anything claimed or
/// heartbeat-live.
pub fn destroy(_handle: &WorktreeHandle) -> CaduceusResult<()> {
    Ok(())
}

/// Worktree GC entry point shared by both `caduceus worktree-gc` and the
/// scheduled background sweep.
pub fn gc(_state_dir: &Path, _older_than_days: u64, _dry_run: bool) -> CaduceusResult<u64> {
    Ok(0)
}
