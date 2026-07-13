//! Queue state and phase/types. Phase 3 fills in crash-safe state I/O.
//! The types here are normative and re-exported from `lib.rs`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::CaduceusResult;
use crate::issue::IssueKey;

/// Phase of one issue in the queue. See `CONTRACTS.md`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
pub enum TicketType {
    Code,
    Investigation,
}

/// Finalization stage. Persisted atomically after every idempotent side
/// effect (see `CONTRACTS.md` "Finalization contract").
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
pub struct QueueState {
    pub version: u32,
    pub entries: BTreeMap<String, QueueEntry>,
}

impl QueueState {
    pub fn empty() -> Self {
        Self {
            version: 1,
            entries: BTreeMap::new(),
        }
    }
}

/// Load the queue state file from disk. Phase 3 owns the atomic I/O.
pub fn load(_path: &PathBuf) -> CaduceusResult<QueueState> {
    Ok(QueueState::empty())
}

/// Persist the queue state file. Phase 3 owns the atomic writer.
pub fn save(_path: &PathBuf, _state: &QueueState) -> CaduceusResult<()> {
    Ok(())
}
