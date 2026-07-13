//! Canonical `<worktree>/worker-prompt.md` writer. Task 4.4 owns the body.

#![allow(dead_code)]

use std::path::Path;

use crate::context::WorkerContext;
use crate::error::CaduceusResult;

/// Write the deterministic prompt for this run into the worktree.
pub fn write(_ctx: &WorkerContext, _worktree: &Path) -> CaduceusResult<()> {
    Ok(())
}
