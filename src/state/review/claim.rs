//! Review claim lifecycle (issue #295): the issue queue's
//! `ClaimToken`/`ClaimFileBody` mirrored for review entries, with two
//! deliberate differences:
//!
//! - claim files live under `<state_dir>/review-claims/` — the issue
//!   reaper parses every `claims/*.claim` as an issue-shaped
//!   `ClaimFileBody` and would quarantine review claims, so the
//!   directories never mix;
//! - the body's identity is a [`ReviewTarget`], not an `IssueKey`.
//!
//! The reaping of stale review claims is dispatch's problem
//! (#312/#339); this change ships the acquire/release primitives.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::review::ReviewTarget;

/// Opaque review claim token, constructed by
/// [`ReviewStore::acquire_next_review`] and consumed by the matching
/// terminal transition. The token's digest is the SHA-256 hex of the
/// canonical review queue key — the same digest used to name the
/// claim file on disk. Mirrors the issue queue's `ClaimToken`.
#[derive(Clone, Debug)]
pub struct ReviewClaimToken {
    claims_dir: PathBuf,
    digest: String,
    run_id: String,
}

impl ReviewClaimToken {
    /// SHA-256 hex of the canonical review queue key — the claim
    /// file's basename stem.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Run identifier recorded in the claim file and checked against
    /// the queue entry's `last_run_id` on every state transition.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Test-only constructor used to exercise the claim-mismatch
    /// rejection path without going through `acquire_next_review`.
    #[doc(hidden)]
    pub fn for_test(claims_dir: PathBuf, digest: &str, run_id: &str) -> Self {
        Self {
            claims_dir,
            digest: digest.to_string(),
            run_id: run_id.to_string(),
        }
    }

    pub(crate) fn claim_path(&self) -> PathBuf {
        self.claims_dir.join(format!("{}.claim", self.digest))
    }
    /// Internal constructor for `ReviewStore::acquire_next_review`
    /// (same crate, different module — fields stay private so tokens
    /// cannot be forged outside the crate).
    pub(crate) fn new(claims_dir: PathBuf, digest: String, run_id: String) -> Self {
        Self {
            claims_dir,
            digest,
            run_id,
        }
    }
}

/// Result of a successful review claim: the (now `InProgress`) entry
/// plus the token every later transition must present.
#[derive(Clone, Debug)]
pub struct ClaimedReview {
    pub entry: crate::state::review::ReviewQueueEntry,
    pub claim: ReviewClaimToken,
}

/// Body of a review claim file. Versioned and `deny_unknown_fields`
/// so a future schema bump is rejected loudly rather than
/// best-effort parsed. Same field set as the issue queue's
/// `ClaimFileBody` with `target: ReviewTarget` replacing
/// `key: IssueKey`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewClaimFileBody {
    pub version: u32,
    pub target: ReviewTarget,
    pub run_id: String,
    pub pid: u32,
    pub process_start_identity: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub worktree_path: Option<PathBuf>,
}

impl ReviewClaimFileBody {
    pub fn version_value(&self) -> u32 {
        crate::state::review::REVIEW_CLAIM_FILE_VERSION
    }
}
