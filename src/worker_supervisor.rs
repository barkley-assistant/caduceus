//! Internal Rust worker supervisor. Spawns the bridge in its own Unix
//! session, forwards timeout/SIGINT/SIGTERM/daemon-parent-death, and drains
//! stdout/stderr. Phase 5 (Task 5.1) owns the body; this stub defines the
//! typed surface.

#![allow(dead_code)]

use std::path::PathBuf;

use tokio::process::Child;

use crate::error::CaduceusResult;
use crate::issue::IssueKey;

/// Outcome of supervising one worker invocation.
#[derive(Debug)]
pub struct SupervisedWorker {
    pub run_id: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub signal: Option<i32>,
}

/// Start the worker under supervision. Tasks 5.1 and 5.2 implement the
/// environment construction + spawn.
pub async fn spawn(_command: &[String], _cwd: &PathBuf, _key: &IssueKey) -> CaduceusResult<Child> {
    Err(crate::error::CaduceusError::Worker {
        context: "spawn",
        stderr: "spawn() implementation lives in Task 5.1".to_string(),
    })
}
