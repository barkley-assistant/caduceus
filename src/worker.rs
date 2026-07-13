//! Worker invocation and result schema.
//!
//! The bridge writes `<worktree>/worker-result.json` on exit 0. The
//! daemon then [`parse_result_file`]s that file — opening it with
//! `O_NOFOLLOW`, verifying the descriptor is a regular file, and
//! reading with a 1 MiB cap before allocating the full document.
//!
//! Every string field is validated:
//!
//! * Trimmed, non-empty, NUL-free.
//! * `summary` ≤ 64 KiB.
//! * `commit_message` and `pull_request_title` ≤ 256 characters.
//! * `pull_request_title` is one line with no control characters.
//! * `commit_message` may contain newlines but no other control
//!   characters.
//!
//! Artifact keys are non-empty, control-free, at most 128 characters,
//! and the map is limited to 100 entries. The map is a
//! `BTreeMap<String, serde_json::Value>` so iteration is stable.
//!
//! Investigation tickets use the same schema: `commit_message` and
//! `pull_request_title` must still be present (schema stability),
//! but the finalization path ignores them. Code tickets require
//! meaningful repository changes later in finalize.
//!
//! All file- and schema-level failures are wrapped as a contextual
//! `CaduceusError::Worker` so the structured logger and the
//! queue retry logic can branch on the operation label.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CaduceusError, CaduceusResult};
use crate::issue::IssueKey;

/// Hard cap on the worker-result file size per `CONTRACTS.md`
/// "Worker environment and result".
pub const MAX_RESULT_FILE_BYTES: u64 = 1 << 20; // 1 MiB

/// Maximum size of the `summary` field.
pub const MAX_SUMMARY_BYTES: usize = 64 * 1024;

/// Maximum size of `commit_message` and `pull_request_title`.
pub const MAX_TITLE_BYTES: usize = 256;

/// Maximum length of an artifact key.
pub const MAX_ARTIFACT_KEY_LEN: usize = 128;

/// Maximum number of artifact entries.
pub const MAX_ARTIFACTS: usize = 100;

/// Result the bridge writes to `<worktree>/worker-result.json`.
///
/// Field semantics and size limits are pinned in `CONTRACTS.md`
/// under "Worker environment and result".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Resolve and validate `worker_command` from config + env. The
/// implementation lives in `validate::resolve_executable`; this
/// helper is retained so the worker module keeps a single entry
/// point. Phase 5 wires the real caller.
pub fn resolve_command(_cwd: &PathBuf) -> CaduceusResult<Vec<String>> {
    Ok(Vec::new())
}

/// Parse + validate a `worker-result.json` file at *path* against
/// the canonical schema. The function performs the read-side
/// invariants the contract requires: `O_NOFOLLOW` open, regular
/// file check, 1 MiB read cap, then JSON parse + validation.
pub fn parse_result_file(path: &Path, issue: &IssueKey) -> CaduceusResult<WorkerResult> {
    let bytes =
        read_capped_file(path, MAX_RESULT_FILE_BYTES).map_err(|err| CaduceusError::Worker {
            context: "read",
            stderr: format!("{}: {err}", path.display()),
        })?;
    let result: WorkerResult =
        serde_json::from_slice(&bytes).map_err(|err| CaduceusError::Worker {
            context: "parse",
            stderr: format!("{}: {err}", path.display()),
        })?;
    validate_worker_result(&result, issue).map_err(|err| CaduceusError::Worker {
        context: "validate",
        stderr: format!("{}: {err}", path.display()),
    })?;
    Ok(result)
}

/// Pure validator: takes an already-parsed [`WorkerResult`] and
/// confirms the document satisfies every field-level rule. Exposed
/// separately so tests can drive the validator without a file.
pub fn validate_worker_result(result: &WorkerResult, _issue: &IssueKey) -> CaduceusResult<()> {
    validate_required_string("summary", &result.summary, MAX_SUMMARY_BYTES)?;
    validate_required_string("commit_message", &result.commit_message, MAX_TITLE_BYTES)?;
    validate_required_string(
        "pull_request_title",
        &result.pull_request_title,
        MAX_TITLE_BYTES,
    )?;
    if contains_control_other_than_newline(&result.commit_message) {
        return Err(CaduceusError::Config(
            "commit_message contains control characters".to_string(),
        ));
    }
    if contains_control(&result.pull_request_title) {
        return Err(CaduceusError::Config(
            "pull_request_title contains control characters".to_string(),
        ));
    }
    if result.pull_request_title.contains('\n') {
        return Err(CaduceusError::Config(
            "pull_request_title must be a single line".to_string(),
        ));
    }
    validate_artifacts(&result.artifacts)?;
    Ok(())
}

fn validate_required_string(field: &str, value: &str, max: usize) -> CaduceusResult<()> {
    if value.contains('\0') {
        return Err(CaduceusError::Config(format!("{field} contains NUL")));
    }
    if value.trim().is_empty() {
        return Err(CaduceusError::Config(format!("{field} is empty")));
    }
    if value.len() > max {
        return Err(CaduceusError::Config(format!(
            "{field} exceeds limit of {max} bytes (got {})",
            value.len()
        )));
    }
    Ok(())
}

fn contains_control(value: &str) -> bool {
    value.chars().any(|c| c.is_control())
}

fn contains_control_other_than_newline(value: &str) -> bool {
    value
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\r')
}

fn validate_artifacts(artifacts: &BTreeMap<String, serde_json::Value>) -> CaduceusResult<()> {
    if artifacts.len() > MAX_ARTIFACTS {
        return Err(CaduceusError::Config(format!(
            "artifacts exceeds limit of {MAX_ARTIFACTS} entries (got {})",
            artifacts.len()
        )));
    }
    for key in artifacts.keys() {
        if key.is_empty() {
            return Err(CaduceusError::Config("artifact key is empty".to_string()));
        }
        if key.len() > MAX_ARTIFACT_KEY_LEN {
            return Err(CaduceusError::Config(format!(
                "artifact key exceeds limit of {MAX_ARTIFACT_KEY_LEN} chars (got {})",
                key.len()
            )));
        }
        if contains_control(key) {
            return Err(CaduceusError::Config(
                "artifact key contains control characters".to_string(),
            ));
        }
    }
    Ok(())
}

/// Open *path* with `O_NOFOLLOW`, verify the resolved descriptor is
/// a regular file, then read at most *cap* bytes. Returns a clean
/// `CaduceusError::Config` for the read-side failures.
fn read_capped_file(path: &Path, cap: u64) -> CaduceusResult<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| CaduceusError::Config(format!("open {}: {err}", path.display())))?;
    let meta = file
        .metadata()
        .map_err(|err| CaduceusError::Config(format!("stat {}: {err}", path.display())))?;
    if !meta.is_file() {
        return Err(CaduceusError::Config(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > cap {
        return Err(CaduceusError::Config(format!(
            "{} exceeds cap of {cap} bytes (got {})",
            path.display(),
            meta.len()
        )));
    }
    let mut buf = Vec::with_capacity(meta.len() as usize);
    let mut handle = file.take(cap);
    handle
        .read_to_end(&mut buf)
        .map_err(|err| CaduceusError::Config(format!("read {}: {err}", path.display())))?;
    Ok(buf)
}
