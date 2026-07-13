//! Worker context JSON. The shape and the serialized form are pinned by
//! `CONTRACTS.md` under "Worker environment and result" / "build stable
//! context JSON" (Task 5.6).

use serde::{Deserialize, Serialize};

use crate::issue::IssueKey;

/// Stable, deterministic context payload delivered to the worker bridge as
/// `CADUCEUS_CONTEXT_JSON` and as a set of `CADUCEUS_*` environment
/// variables.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerContext {
    pub issue: IssueKey,
    pub issue_title: String,
    pub issue_body: String,
    pub labels: Vec<String>,
    pub worktree_path: std::path::PathBuf,
    pub run_id: String,
    pub branch_name: String,
}
