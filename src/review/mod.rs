//! Auto Review domain model — the typed foundation for the review
//! pipeline (epic #290, spec `docs/architecture/auto-review.md`).
//!
//! This module owns the review domain's data shapes and their
//! parse-time contracts only. It deliberately contains no I/O, no
//! store, and no execution logic:
//!
//! - [`ReviewTarget`] — immutable review identity
//!   `(repository, PR number, head SHA)` frozen at discovery, plus the
//!   diff context (`base_sha`, `base_ref`, `merge_base`) computed and
//!   persisted at admission (DAR §2.1).
//! - [`ReviewState`] — per-`(repo, pr)` durable current-state pointer,
//!   including the monotonic `review_generation` publication guard
//!   (DAR §9.4).
//! - [`ReviewResult`] / [`Review`] / [`Finding`] — the per-run worker
//!   result contract (DAR §3). `ReviewResult` is the only review type
//!   carrying `schema_version`: it crosses a process boundary and is
//!   persisted as an opaque version-tagged history blob (DAR §4.3).
//!
//! Diff semantics (DAR §2.2): the review scope is always
//! `git diff <merge_base> <head_sha>` — merge-base (three-dot)
//! semantics. Endpoint-to-endpoint `base..head` ranges are prohibited;
//! no helper in this module may format a `..`/`...` range string, and
//! `base_sha` is context, never a diff endpoint.
//!
//! Out of scope here (later issues): persistence (#295), migrations
//! (#293), the full result validator — verdict/severity consistency,
//! status/review presence, line/path semantics (#305) — executor
//! targets (#346), CLI (#318).

use serde::{Deserialize, Serialize};

use crate::infra::error::{CaduceusError, CaduceusResult};

/// Schema version of the [`ReviewResult`] document. Bumped on any
/// breaking change to the wire shape. The v1 parser
/// ([`parse_review_result`]) rejects documents carrying any other
/// value; version-aware reading of old history blobs arrives with the
/// store (#295).
pub const REVIEW_SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Field caps (DAR §3, §10.3)
//
// Caps are byte budgets (UTF-8), matching the byte-budget world of the
// sticky-comment renderer (DAR §9.2). Tight where the field is
// worker-produced (adversarial surface; rejections burn retry budget
// per DAR §8, so limits are generous enough to never reject a sane
// result), loose where the field is daemon-authored (storage bounds
// only). Content-shape rules beyond length (hex form, ref syntax,
// GitHub naming) belong to the #305 validator.
// ---------------------------------------------------------------------------

/// `Review.summary` — mirrors `MAX_SUMMARY_BYTES`
/// (`src/worker/worker_contract.rs`).
pub const MAX_REVIEW_SUMMARY_BYTES: usize = 64 * 1024;
/// Maximum number of findings in one review — mirrors `MAX_ARTIFACTS`.
pub const MAX_FINDINGS: usize = 100;
/// `Finding.title` — mirrors `MAX_PULL_REQUEST_TITLE_CHARS`.
pub const MAX_FINDING_TITLE_BYTES: usize = 256;
/// `Finding.body` — a single finding must not approach the 64 KiB
/// comment budget on its own.
pub const MAX_FINDING_BODY_BYTES: usize = 16 * 1024;
/// `Finding.remediation` — guidance text, shorter than the body.
pub const MAX_FINDING_REMEDIATION_BYTES: usize = 8 * 1024;
/// `Finding.path` — PATH_MAX.
pub const MAX_FINDING_PATH_BYTES: usize = 4096;
/// SHA fields (`head_sha`, `base_sha`, `merge_base`) — fits SHA-1 (40)
/// and SHA-256 (64) hex forms.
pub const MAX_SHA_BYTES: usize = 64;
/// `base_ref` — storage bound; ref-syntax validation is #297/#305.
pub const MAX_REF_BYTES: usize = 1024;
/// `RepositoryId.owner` / `.repo` — coarse storage bound; GitHub
/// naming rules are #305's validator.
pub const MAX_REPO_COMPONENT_BYTES: usize = 256;
/// `ReviewState.last_run_id` — mirrors `validate_run_id`
/// (`src/worktree/worktree.rs`).
pub const MAX_RUN_ID_BYTES: usize = 64;
/// `ReviewState.last_publish_error` — bounded logging surface;
/// generous to never reject daemon-authored state.
pub const MAX_PUBLISH_ERROR_BYTES: usize = 4096;

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// GitHub repository identity for the review domain: `(owner, repo)`.
///
/// Deliberately NOT [`crate::github::issue::IssueKey`]: review identity
/// never enters `IssueKey` (DAR §4.1) — a PR review is not an issue,
/// and this type cannot be used as one without the caller explicitly
/// constructing a separate key. Cheap by design: no registry, no I/O,
/// no case normalisation (canonical comparison is a store-layer
/// concern, #295).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub struct RepositoryId {
    /// Repository owner (user or organisation login).
    pub owner: String,
    /// Repository name.
    pub repo: String,
}

impl RepositoryId {
    /// `owner/repo` display form. No case normalisation — callers that
    /// need canonical comparison normalise at their own layer.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// Immutable review revision identity, frozen at discovery (DAR §2.1).
///
/// Identity = `(repository, pull_request, head_sha)`. The head SHA is
/// captured at discovery and never re-resolved; if the PR moves on
/// while a review runs, the next poll admits the new SHA as a new
/// target. `base_sha`, `base_ref`, and `merge_base` are **context**,
/// not identity: base movement with an unchanged head SHA is not a new
/// review.
///
/// Diff rule (DAR §2.2): review scope is always
/// `git diff <merge_base> <head_sha>`. Never `base..head`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub struct ReviewTarget {
    /// Repository the PR lives in.
    pub repository: RepositoryId,
    /// PR number.
    pub pull_request: u64,
    /// Head SHA at discovery — the identity component.
    pub head_sha: String,
    /// Base SHA — context for merge-base computation; never a diff
    /// endpoint.
    pub base_sha: String,
    /// Base ref name — context.
    pub base_ref: String,
    /// Merge base of `base_sha` and `head_sha`, computed once at
    /// admission and persisted here (DAR §2.1). Frozen context: all
    /// later diff computation and CLI display reuse this value.
    pub merge_base: String,
}

// ---------------------------------------------------------------------------
// Parse-time contracts (caps + schema version + non-empty)
// ---------------------------------------------------------------------------

/// Required string: non-empty and at most `max` bytes.
fn required_string(scope: &str, field: &str, value: &str, max: usize) -> CaduceusResult<()> {
    if value.is_empty() {
        return Err(CaduceusError::Config(format!(
            "{scope}: {field} must not be empty"
        )));
    }
    if value.len() > max {
        return Err(CaduceusError::Config(format!(
            "{scope}: {field} exceeds limit of {max} bytes (got {})",
            value.len()
        )));
    }
    Ok(())
}

/// Cap validation for a [`ReviewTarget`] (the store calls this on
/// load, #295).
pub fn validate_review_target(target: &ReviewTarget) -> CaduceusResult<()> {
    required_string(
        "review target",
        "repository.owner",
        &target.repository.owner,
        MAX_REPO_COMPONENT_BYTES,
    )?;
    required_string(
        "review target",
        "repository.repo",
        &target.repository.repo,
        MAX_REPO_COMPONENT_BYTES,
    )?;
    required_string("review target", "head_sha", &target.head_sha, MAX_SHA_BYTES)?;
    required_string("review target", "base_sha", &target.base_sha, MAX_SHA_BYTES)?;
    required_string("review target", "base_ref", &target.base_ref, MAX_REF_BYTES)?;
    required_string(
        "review target",
        "merge_base",
        &target.merge_base,
        MAX_SHA_BYTES,
    )?;
    Ok(())
}
