//! Append-only review history (issue #295, DAR §4.3): one row per
//! completed run, identity = `review_run_id`; `(repo, pr, head_sha)`
//! is deliberately NOT unique.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::review::RepositoryId;

/// One completed review run (DAR §4.3). Identity =
/// `review_run_id`, unique per COMPLETED run; `(repo, pr, head_sha)`
/// is NOT unique — same-SHA re-review appends additional rows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewHistoryRow {
    /// The queue claim's run id (execution identity).
    pub review_run_id: String,
    /// Repository the PR lives in. Stored verbatim (no case
    /// normalisation); queries go through the key columns.
    pub repository: RepositoryId,
    pub pull_request: u64,
    pub head_sha: String,
    /// The generation the run completed under.
    pub review_generation: u64,
    pub completed_at: DateTime<Utc>,
    /// Verbatim serialized `ReviewResult` document
    /// (version-tagged by its own `schema_version`). This is the
    /// canonical durable result (DAR §4.3); the PR comment is
    /// presentation derived from it, never the reverse. Old versions
    /// are read-only and never back-migrated.
    pub result_json: String,
}

/// Versioned review history file: append order is preserved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewHistoryFile {
    pub version: u32,
    pub rows: Vec<ReviewHistoryRow>,
}

impl ReviewHistoryFile {
    pub fn empty() -> Self {
        Self {
            version: crate::state::review::REVIEW_HISTORY_FILE_VERSION,
            rows: Vec::new(),
        }
    }
}
