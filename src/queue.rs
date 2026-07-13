//! Queue state and phase types. The `IssueKey` type lives in
//! [`crate::issue`] but its parsing and the queue's versioned
//! envelope are owned by this module per `CONTRACTS.md` "Issue
//! identity and queue schema".
//!
//! Serialization is schema-stable:
//!
//! * Every struct uses `deny_unknown_fields` so the daemon refuses
//!   a schema it does not know — operators must upgrade the daemon
//!   before an upgrade to the queue file format can land.
//! * Timestamps are RFC-3339 with the UTC offset (chrono emits
//!   `2026-07-13T14:23:45.123Z` by default for `DateTime<Utc>`).
//! * Phase / TicketType / FinalizationStage use `snake_case`
//!   serde renaming.
//! * A future version of the queue file produces a
//!   `CaduceusError::StateCorrupt` — never best-effort parsing.
//!
//! Test seam: [`parse_queue_state`] accepts any string so tests can
//! drive the schema directly without touching the filesystem.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{CaduceusError, CaduceusResult};
use crate::issue::IssueKey;

/// Canonical queue-file schema version. Bumping it is a breaking
/// change — the daemon refuses any other value. Tested by
/// [`tests/queue_model_test.rs`].
pub const QUEUE_FILE_VERSION: u32 = 1;

/// Phase of one issue in the queue. See `CONTRACTS.md`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Phase {
    Queued,
    InProgress,
    Previewed,
    Done,
    Failed,
    Skipped,
}

/// Ticket kind selected by the trigger label.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum TicketType {
    Code,
    Investigation,
}

/// Finalization stage. Persisted atomically after every idempotent side
/// effect (see `CONTRACTS.md` "Finalization contract").
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum FinalizationStage {
    Committed,
    Pushed,
    PrCreated,
    Commented,
    InvestigationReady,
    InvestigationCommented,
}

/// Checkpoint used for crash-safe resumption of finalization.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizationCheckpoint {
    pub run_id: String,
    pub branch_name: String,
    pub result_path: PathBuf,
    pub stage: FinalizationStage,
    pub commit_oid: Option<String>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
}

/// Single queue entry. All state for one `owner/repo#number`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueEntry {
    pub key: IssueKey,
    pub phase: Phase,
    pub ticket_type: TicketType,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub last_run_id: Option<String>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub finalization: Option<FinalizationCheckpoint>,
    pub queued_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Versioned queue file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueState {
    pub version: u32,
    pub entries: BTreeMap<String, QueueEntry>,
}

impl QueueState {
    pub fn empty() -> Self {
        Self {
            version: QUEUE_FILE_VERSION,
            entries: BTreeMap::new(),
        }
    }

    /// Borrow the entry keyed by the given (validated) ``IssueKey``,
    /// using its lowercase display form to look up. Returns
    /// [`CaduceusError::Other`] when the key fails validation
    /// (which should never happen for an `IssueKey` constructed in
    /// memory — but the helper keeps the failure mode deterministic
    /// if a future caller slips through).
    pub fn entry(&self, key: &IssueKey) -> Option<&QueueEntry> {
        key.validate().ok()?;
        self.entries.get(&key.display_key())
    }
}

/// Parse + validate a queue file from text. The function is the
/// canonical reader; production code that loads from disk calls
/// this after reading the file. Tests drive it directly so they
/// don't need a temp file for every schema assertion.
pub fn parse_queue_state(text: &str) -> CaduceusResult<QueueState> {
    let state: QueueState =
        serde_json::from_str(text).map_err(|err| CaduceusError::StateCorrupt {
            path: PathBuf::from("<queue-state>"),
            message: format!("queue state JSON parse: {err}"),
        })?;
    if state.version != QUEUE_FILE_VERSION {
        return Err(CaduceusError::StateCorrupt {
            path: PathBuf::from("<queue-state>"),
            message: format!(
                "unsupported queue state version: got {}, expected {}",
                state.version, QUEUE_FILE_VERSION
            ),
        });
    }
    // Every map key must be the lowercase display form of its
    // entry's IssueKey; this catches the "matched casing" case
    // where someone re-keys the file.
    for (map_key, entry) in &state.entries {
        if map_key != &entry.key.display_key() {
            return Err(CaduceusError::StateCorrupt {
                path: PathBuf::from("<queue-state>"),
                message: format!(
                    "queue map key {map_key:?} does not match entry {}",
                    entry.key.display_key()
                ),
            });
        }
        entry
            .key
            .validate()
            .map_err(|err| CaduceusError::StateCorrupt {
                path: PathBuf::from("<queue-state>"),
                message: format!("queue entry key invalid: {err:?}"),
            })?;
    }
    Ok(state)
}

/// Serialize the queue state to canonical JSON. The result is a
/// one-line document with no extraneous whitespace so stable
/// hashing and diff-friendly storage stay simple.
pub fn serialize_queue_state(state: &QueueState) -> CaduceusResult<String> {
    serde_json::to_string(state).map_err(|err| CaduceusError::StateCorrupt {
        path: PathBuf::from("<queue-state>"),
        message: format!("queue state JSON serialize: {err}"),
    })
}

/// Load the queue state file from disk. Phase 3 owns the atomic I/O.
pub fn load(_path: &PathBuf) -> CaduceusResult<QueueState> {
    Ok(QueueState::empty())
}

/// Persist the queue state file. Phase 3 owns the atomic writer.
pub fn save(_path: &PathBuf, _state: &QueueState) -> CaduceusResult<()> {
    Ok(())
}
