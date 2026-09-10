//! Review-era persistence (issue #295, DAR §4.1-4.3).
//!
//! Three sibling stores live beside the issue queue, one per surface,
//! each with a JSON mirror file and a SQLite table in the same
//! `state.db`:
//!
//! - review queue (`review_queue.json` / `review_queue_entries`) —
//!   entries keyed by the canonical [`review_queue_key`] of a
//!   persisted [`ReviewTarget`];
//! - per-`(repo, pr)` state (`review_state.json` / `review_state`) —
//!   keyed by the canonical lowercase [`review_state_key`];
//! - append-only history (`review_history.json` / `review_history`) —
//!   identity = `review_run_id`, unique per completed run;
//!   `(repo, pr, head_sha)` is deliberately NOT unique.
//!
//! Version policy: these files are born at v1 in the same change as
//! their first reader, so there is no legitimate older version — a
//! parse whose envelope version differs from the module constant in
//! EITHER direction is [`CaduceusError::StoreVersionUnsupported`].
//! The review-era activation that makes this module live is the
//! store-envelope bump v7→v8 plus the `state.json` envelope bump to
//! `QUEUE_FILE_VERSION = 2` (see [`crate::state::store`]).
//!
//! Lock discipline: JSON mutations serialise on an exclusive `flock`
//! of `<state_dir>/review.lock` — deliberately NOT the issue queue's
//! `state.lock`, so the two failure domains never nest. Review claim
//! files live under `<state_dir>/review-claims/` so the issue-queue
//! reaper (which parses every `claims/*.claim` as an issue-shaped
//! `ClaimFileBody`) never sees them.

mod claim;
mod history;
mod queue;
mod state;

use std::path::PathBuf;

use crate::infra::error::{store_version_guidance, CaduceusError, CaduceusResult};
use crate::review::{
    RepositoryId, ReviewResult, MAX_REPO_COMPONENT_BYTES, MAX_RUN_ID_BYTES, MAX_SHA_BYTES,
    REVIEW_SCHEMA_VERSION,
};

pub use claim::{ClaimedReview, ReviewClaimFileBody, ReviewClaimToken};
pub use history::{ReviewHistoryFile, ReviewHistoryRow};
pub use queue::{
    EnqueueReason, ReviewEnqueueOutcome, ReviewPhase, ReviewQueueEntry, ReviewQueueState,
};
pub use state::{
    parse_review_history, parse_review_queue_state, parse_review_state_map,
    serialize_review_history, serialize_review_queue_state, serialize_review_state_map,
    ReviewStateMap, ReviewStore,
};

use crate::review::ReviewTarget;

/// Envelope version of `review_queue.json`. Born at v1 with its first
/// reader in this change.
pub const REVIEW_QUEUE_FILE_VERSION: u32 = 1;
/// Envelope version of `review_state.json`. Born at v1 with its first
/// reader in this change.
pub const REVIEW_STATE_FILE_VERSION: u32 = 1;
/// Envelope version of `review_history.json`. Born at v1 with its
/// first reader in this change.
pub const REVIEW_HISTORY_FILE_VERSION: u32 = 1;
/// On-disk format of a review claim file.
pub const REVIEW_CLAIM_FILE_VERSION: u32 = 1;

/// Name of the review queue file inside `<state_dir>`.
pub const REVIEW_QUEUE_FILENAME: &str = "review_queue.json";
/// Name of the review state file inside `<state_dir>`.
pub const REVIEW_STATE_FILENAME: &str = "review_state.json";
/// Name of the review history file inside `<state_dir>`.
pub const REVIEW_HISTORY_FILENAME: &str = "review_history.json";
/// Name of the review claims directory inside `<state_dir>`.
pub const REVIEW_CLAIMS_DIRNAME: &str = "review-claims";
/// Name of the `flock` file serialising review-store mutations.
/// Distinct from the issue queue's `state.lock` (never nested).
pub const REVIEW_LOCK_FILENAME: &str = "review.lock";

/// Canonical lowercase key for a `(repo, pr)` pair — the JSON
/// `review_state.json` map key and the normalised identity used by
/// the SQLite `review_state` primary key. Mirrors the issue queue's
/// lowercase display-key rule: GitHub owner/repo are case-insensitive
/// and the store is the normalisation layer (`RepositoryId`
/// deliberately does no case folding).
pub fn review_state_key(repository: &RepositoryId, pull_request: u64) -> String {
    format!(
        "{}/{}#{}",
        repository.owner.to_lowercase(),
        repository.repo.to_lowercase(),
        pull_request
    )
}

/// Canonical key for a review queue entry: the state key plus `"@"`
/// and the head SHA. The head SHA is already case-normalised hex from
/// git and is stored verbatim.
pub fn review_queue_key(target: &ReviewTarget) -> String {
    format!(
        "{}@{}",
        review_state_key(&target.repository, target.pull_request),
        target.head_sha
    )
}

/// SHA-256 hex digest of a canonical review key — the review claim
/// file's basename. Same construction as the issue queue's
/// `display_digest`.
pub fn review_claim_digest(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Enforce a review-file envelope version: these files are born at v1
/// with their first reader, so any other value — older OR newer —
/// means a different-era binary wrote the file.
pub(crate) fn require_review_file_version(
    file: &str,
    found: u32,
    supported: u32,
) -> CaduceusResult<()> {
    if found != supported {
        let (guidance, found, supported) = if found > supported {
            (store_version_guidance(true), found as i64, supported as i64)
        } else {
            (
                store_version_guidance(false),
                found as i64,
                supported as i64,
            )
        };
        return Err(CaduceusError::StoreVersionUnsupported {
            backend: "json",
            path: PathBuf::from(file),
            found,
            supported,
            guidance,
        });
    }
    Ok(())
}

/// Required string: non-empty and at most `max` bytes (shared shape
/// with the review domain validators, surfaced as `StateCorrupt`).
pub(crate) fn bounded_string(
    scope: &str,
    field: &str,
    value: &str,
    max: usize,
) -> CaduceusResult<()> {
    if value.is_empty() {
        return Err(CaduceusError::StateCorrupt {
            path: PathBuf::from(scope),
            message: format!("{field} must not be empty"),
        });
    }
    if value.len() > max {
        return Err(CaduceusError::StateCorrupt {
            path: PathBuf::from(scope),
            message: format!("{field} exceeds limit of {max} bytes (got {})", value.len()),
        });
    }
    Ok(())
}

/// Validate one history row's identity fields and durable result
/// blob. History rows carry no base/merge_base context, so the target
/// caps apply to the identity components only. The blob must be a
/// JSON object carrying an integer `schema_version` — corruption is
/// caught at append time by the writer, not the reader.
/// Current-version rows must parse as a full `ReviewResult`; older
/// versions are accepted as opaque, read-only blobs (DAR §4.3: old
/// versions are never back-migrated).
pub(crate) fn validate_history_row(row: &ReviewHistoryRow) -> CaduceusResult<()> {
    const SCOPE: &str = "<review-history>";
    bounded_string(
        SCOPE,
        "repository.owner",
        &row.repository.owner,
        MAX_REPO_COMPONENT_BYTES,
    )?;
    bounded_string(
        SCOPE,
        "repository.repo",
        &row.repository.repo,
        MAX_REPO_COMPONENT_BYTES,
    )?;
    bounded_string(SCOPE, "head_sha", &row.head_sha, MAX_SHA_BYTES)?;
    bounded_string(SCOPE, "review_run_id", &row.review_run_id, MAX_RUN_ID_BYTES)?;

    let value: serde_json::Value =
        serde_json::from_str(&row.result_json).map_err(|err| CaduceusError::StateCorrupt {
            path: PathBuf::from(SCOPE),
            message: format!("result_json is not valid JSON: {err}"),
        })?;
    let schema_version = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| CaduceusError::StateCorrupt {
            path: PathBuf::from(SCOPE),
            message: "result_json must be an object with an integer schema_version".to_string(),
        })?;
    if schema_version == REVIEW_SCHEMA_VERSION as u64 {
        let result: ReviewResult =
            serde_json::from_str(&row.result_json).map_err(|err| CaduceusError::StateCorrupt {
                path: PathBuf::from(SCOPE),
                message: format!("current-version result_json is not a valid ReviewResult: {err}"),
            })?;
        crate::review::validate_review_result(&result)?;
    }
    Ok(())
}

/// Map a strict-caps [`crate::review::ReviewState`] validation error
/// into the store-layer corruption error (the domain validator
/// reports via `Config`; the store surface is `StateCorrupt`).
pub(crate) fn validated_review_state(state: &crate::review::ReviewState) -> CaduceusResult<()> {
    crate::review::validate_review_state(state).map_err(|e| CaduceusError::StateCorrupt {
        path: PathBuf::from("<review-store>"),
        message: format!("review state invalid: {e}"),
    })
}
