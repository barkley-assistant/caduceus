//! `caduceus status` reporter. Reads queue + metadata + heartbeats through
//! the normal resolution chain. Task 7.3 owns the body.

#![allow(dead_code)]

use std::path::PathBuf;

use serde::Serialize;

use crate::error::CaduceusResult;

/// Structured status payload used by both human and `--json` output.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub version: String,
    pub state_dir: PathBuf,
    pub last_tick: Option<String>,
    pub last_outcome: Option<String>,
    pub phases: std::collections::BTreeMap<String, u64>,
    pub next_head: Option<String>,
    pub recent_errors: Vec<String>,
    pub rate_limit: Option<super::meta::RateLimitState>,
    pub current_run: Option<CurrentRun>,
}

/// Single currently running worker.
#[derive(Debug, Serialize)]
pub struct CurrentRun {
    pub run_id: String,
    pub issue: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub transcript_path: PathBuf,
}

/// Render a status report for the given state directory.
pub fn report(_state_dir: &PathBuf, _json: bool) -> CaduceusResult<String> {
    Ok(String::from("status: stub\n"))
}
