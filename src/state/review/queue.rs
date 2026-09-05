//! The sibling review queue: entries keyed by [`ReviewTarget`] with a
//! lifecycle mirroring the issue queue's, minus the issue-only
//! variants (issue #295, DAR §4.1).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::review::ReviewTarget;

/// Phase of one review target in the review queue. Mirrors the issue
/// queue's `Phase` lifecycle minus issue-only variants (`Previewed`
/// and `AwaitingReview` are issue-flow concepts). Serde labels are
/// snake_case and double as the SQLite column encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum ReviewPhase {
    Queued,
    InProgress,
    Done,
    Failed,
    Skipped,
    NeedsAttention,
}

impl ReviewPhase {
    /// Stable snake_case label, mirroring `Phase::as_str`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::NeedsAttention => "needs_attention",
        }
    }

    /// Parse the stable label. Unknown labels are a corruption error
    /// at the store layer, never a best-effort guess.
    pub fn from_label(label: &str) -> Option<Self> {
        Some(match label {
            "queued" => Self::Queued,
            "in_progress" => Self::InProgress,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            "needs_attention" => Self::NeedsAttention,
            _ => return None,
        })
    }

    /// Active (non-terminal) phases: an entry in one of these is
    /// "in flight" for same-SHA dedup.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::InProgress)
    }
}

/// One review queue entry, keyed by the canonical
/// [`crate::state::review::review_queue_key`] of its persisted
/// [`ReviewTarget`] (which carries the `merge_base` context verbatim,
/// DAR §2.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewQueueEntry {
    pub target: ReviewTarget,
    pub phase: ReviewPhase,
    /// Execution attempts (queue-owned; `ReviewState` has none).
    pub attempts: u32,
    pub last_error: Option<String>,
    pub last_run_id: Option<String>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub queued_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Monotonic per-`(repo, pr)` generation, assigned at admission —
    /// must equal the `ReviewState` row's `review_generation`
    /// (DAR §9.4).
    pub review_generation: u64,
}

/// Versioned review queue file (mirror of the issue queue's
/// `QueueState` shape).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewQueueState {
    pub version: u32,
    pub entries: BTreeMap<String, ReviewQueueEntry>,
}

impl ReviewQueueState {
    pub fn empty() -> Self {
        Self {
            version: crate::state::review::REVIEW_QUEUE_FILE_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

/// Outcome of a review enqueue. Review-local by design — the issue
/// queue's `EnqueueOutcome::Promoted` is `Previewed`-specific and has
/// no review meaning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewEnqueueOutcome {
    Inserted,
    AlreadyPresent,
}
