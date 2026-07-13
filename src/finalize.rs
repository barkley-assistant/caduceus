//! Finalization: commit, push, PR, comment/close, investigation comment.
//!
//! Idempotency across partial failures is the hard requirement — see
//! `CONTRACTS.md` "Finalization contract" and Tasks 6.1–6.5. This module
//! only defines the typed surface for now; runtime bodies land in
//! Phase 6.

#![allow(dead_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::CaduceusResult;
use crate::issue::IssueKey;

/// Finalized result handed to the daemon by the worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FinalizeRequest {
    pub issue: IssueKey,
    pub branch_name: String,
    pub worktree_path: PathBuf,
}

/// Outcome of a finalization attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FinalizeOutcome {
    pub commit_oid: Option<String>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
}

/// Dispatcher used by the orchestration loop. Phase 6 owns the real
/// implementation; the stub keeps the symbol reachable.
pub async fn finalize(_req: FinalizeRequest) -> CaduceusResult<FinalizeOutcome> {
    Ok(FinalizeOutcome {
        commit_oid: None,
        pr_number: None,
        pr_url: None,
    })
}
