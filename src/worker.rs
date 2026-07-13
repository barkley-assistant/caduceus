//! Worker invocation and result schema. Phase 5 fills in parsing,
//! validation, and JSON rendering; this stub fixes the shape so the
//! `WorkerResult` re-export compiles cleanly.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::CaduceusResult;
use crate::issue::IssueKey;

/// Result the bridge writes to `<worktree>/worker-result.json`.
///
/// Field semantics and size limits are pinned in `CONTRACTS.md` under
/// "Worker environment and result" / "Filesystem permissions".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerResult {
    pub status: WorkerStatus,
    pub summary: String,
    pub commit_message: String,
    pub pull_request_title: String,
    #[serde(default)]
    pub artifacts: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub investigation: bool,
}

/// Status the bridge can return.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Success,
    Failure,
}

/// Resolve and validate `worker_command` from config + env. Task 1.6 fills
/// the body in.
pub fn resolve_command(_cwd: &PathBuf) -> CaduceusResult<Vec<String>> {
    Ok(Vec::new())
}

/// Parse a `worker-result.json` file into a strongly-typed `WorkerResult`.
pub fn parse_result(_path: &PathBuf, _issue: &IssueKey) -> CaduceusResult<WorkerResult> {
    Ok(WorkerResult {
        status: WorkerStatus::Success,
        summary: String::new(),
        commit_message: String::new(),
        pull_request_title: String::new(),
        artifacts: BTreeMap::new(),
        investigation: false,
    })
}
