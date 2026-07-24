//! Queue state, claim token, and the crash-safe [`StateStore`]. The
//! `IssueKey` type lives in [`crate::issue`] but its parsing and the
//! queue's versioned envelope are owned by this module per
//! `CONTRACTS.md` "Issue identity and queue schema".
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
//!
//! ## Crash-safety
//!
//! Every mutating [`StateStore`] operation takes an exclusive `flock`
//! on `<state_dir>/state.lock`, loads and validates the current state
//! file, applies exactly one transition, persists the result through
//! the standard write-temp → `sync_all` → rename → sync-dir pattern,
//! and finally releases the lock. [`StateStore::snapshot`] uses a
//! shared lock and never rewrites state. Claim files under
//! `<state_dir>/claims/<sha256>.claim` are created with `O_CREAT |
//! O_EXCL` so two concurrent claim attempts cannot both win.
//!
//! Errors during a mutating operation leave the prior file intact:
//! the new state is only visible after a successful rename. A
//! claim-unlink failure at the tail of a transition is reported to
//! the caller but does not roll back the durable phase change — the
//! reaper cleans up orphaned claims idempotently.

#![allow(dead_code)]
#![allow(unused_imports)]

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::github::issue::IssueKey;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Canonical queue-file schema version. Bumping it is a breaking
/// change — the daemon refuses any other value. Tested by
/// [`tests/state/queue_model_test.rs`].
pub const QUEUE_FILE_VERSION: u32 = 1;

/// Name of the queue state file inside `<state_dir>`.
pub const STATE_FILENAME: &str = "state.json";

/// Name of the directory holding per-claim files inside `<state_dir>`.
pub const CLAIMS_DIRNAME: &str = "claims";

/// Name of the `flock` used to serialise mutating [`StateStore`]
/// operations. Distinct from the daemon-wide `daemon.lock` declared
/// in CONTRACTS.md invariant #1, which covers a whole tick.
pub const STATE_LOCK_FILENAME: &str = "state.lock";

/// Name of the claim file's on-disk format. Bumping it is a breaking
/// change — older files are quarantined by the reaper.
pub const CLAIM_FILE_VERSION: u32 = 1;

/// Phase of one issue in the queue. See `CONTRACTS.md`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Phase {
    Queued,
    InProgress,
    Previewed,
    /// The daemon posted a completion comment and is waiting for
    /// human review of the PR before closing. The entry is polled
    /// by [`poll_awaiting_review_entries`] on each tick.
    AwaitingReview,
    Done,
    Failed,
    Skipped,
    /// Conflicting remote markers or ambiguous side effects
    /// during reconciliation. The operator must inspect and
    /// manually resolve before the entry can be re-queued.
    /// Added by Task 4.2 (FINAL-001 contract).
    NeedsAttention,
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
    ResultValidated,
    Committed,
    Pushed,
    PrCreated,
    Commented,
    AwaitingReview,
    Done,
    InvestigationReady,
    InvestigationCommented,
}

impl FinalizationStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ResultValidated => "result_validated",
            Self::Committed => "committed",
            Self::Pushed => "pushed",
            Self::PrCreated => "pr_created",
            Self::Commented => "commented",
            Self::AwaitingReview => "awaiting_review",
            Self::Done => "done",
            Self::InvestigationReady => "investigation_ready",
            Self::InvestigationCommented => "investigation_commented",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "result_validated" => Self::ResultValidated,
            "committed" => Self::Committed,
            "pushed" => Self::Pushed,
            "pr_created" => Self::PrCreated,
            "commented" => Self::Commented,
            "awaiting_review" => Self::AwaitingReview,
            "done" => Self::Done,
            "investigation_ready" => Self::InvestigationReady,
            "investigation_commented" => Self::InvestigationCommented,
            _ => return None,
        })
    }
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
    pub generation: u32,
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

    /// Mutable analogue of [`QueueState::entry`].
    pub fn entry_mut(&mut self, key: &IssueKey) -> Option<&mut QueueEntry> {
        key.validate().ok()?;
        self.entries.get_mut(&key.display_key())
    }
}

/// Outcome of [`StateStore::enqueue`]. Discriminates the three
/// transitions the polling loop can take: a fresh insert, a no-op
/// against an existing entry, or a `Previewed → Queued` promotion
/// when dry-run is disabled and the entry is still labeled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueOutcome {
    Inserted,
    AlreadyPresent,
    Promoted,
}

/// Result of [`StateStore::acquire_next`]. Carries both the entry
/// the daemon will hand to the worker and the matching [`ClaimToken`]
/// every later state transition must reference.
#[derive(Clone, Debug)]
pub struct ClaimedEntry {
    pub entry: QueueEntry,
    pub claim: ClaimToken,
}

/// Result of [`StateStore::reset_entry`]. The caller (the
/// `caduceus queue reset` CLI) renders the dropped checkpoint as
/// a warning so the operator can reconcile the branch / PR
/// manually. `cleared_finalization` is `true` when the
/// `--force-finalization-reset` flag was supplied.
#[derive(Clone, Debug)]
pub struct ResetOutcome {
    pub cleared_finalization: bool,
    pub dropped_checkpoint: Option<FinalizationCheckpoint>,
}

/// Opaque claim token. Constructed by [`StateStore::acquire_next`]
/// and consumed by the matching terminal transition
/// ([`StateStore::complete`], [`StateStore::retry_or_fail`], …).
///
/// The token's digest is the SHA-256 hex of the lowercase display
/// key — the same digest used to name the claim file on disk. The
/// token owns its own copy of the claims directory so the matching
/// write at completion does not need the original `StateStore`
/// instance.
#[derive(Clone, Debug)]
pub struct ClaimToken {
    claims_dir: PathBuf,
    digest: String,
    run_id: String,
}

impl ClaimToken {
    /// Lowercase display-key SHA-256 hex used as the claim file name.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Run identifier recorded in the claim file and checked against
    /// the queue entry's `last_run_id` on every state transition.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Test-only constructor used to exercise the
    /// `set_worktree`/`save_finalization` "wrong token" rejection
    /// path without going through `acquire_next`.
    #[doc(hidden)]
    pub fn for_test(claims_dir: PathBuf, digest: &str, run_id: &str) -> Self {
        Self {
            claims_dir,
            digest: digest.to_string(),
            run_id: run_id.to_string(),
        }
    }

    fn claim_path(&self) -> PathBuf {
        self.claims_dir.join(format!("{}.claim", self.digest))
    }
}

/// Body of a claim file. Versioned and `deny_unknown_fields` so a
/// future schema bump is rejected loudly rather than best-effort
/// parsed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimFileBody {
    pub version: u32,
    pub key: IssueKey,
    pub run_id: String,
    pub pid: u32,
    pub process_start_identity: String,
    pub started_at: DateTime<Utc>,
    pub worktree_path: Option<PathBuf>,
}

fn process_start_identity(pid: u32) -> String {
    // Best-effort composite; on Linux combine boot id + /proc start
    // ticks. The reaper re-validates identity before trusting a
    // claim, so an empty/fallback value is acceptable here as long
    // as we surface what we have.
    let boot = read_boot_id().unwrap_or_else(|| "<unknown-boot>".to_string());
    let start = read_proc_start_ticks(pid).unwrap_or(0u64);
    format!("{boot}:{start}")
}

fn read_boot_id() -> Option<String> {
    let body = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(body.trim().to_string())
}

fn read_proc_start_ticks(pid: u32) -> Option<u64> {
    // Field 22 of /proc/<pid>/stat is starttime in clock ticks.
    let body = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_paren = body.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_paren.split_whitespace().collect();
    // (state) consumes fields 1-2; field index 21 (0-based after the
    // closing paren) maps to starttime.
    let starttime = fields.get(20).copied()?;
    starttime.parse::<u64>().ok()
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

/// Crash-safe persistent state for the queue, claim files, and the
/// phase transitions the daemon drives.
///
/// The store serialises mutating operations under an exclusive
/// `flock` on `<state_dir>/state.lock`. Concurrent [`StateStore`]
/// instances pointing at the same `<state_dir>` see the queue as
/// strictly serialised; the lock is *not* the daemon-wide
/// `daemon.lock`, which covers an entire tick.
///
/// `snapshot` is the only operation that never rewrites the file.
/// Everything else follows the standard write-temp → fsync → rename
/// → fsync-dir pattern.
#[derive(Debug, Clone)]
pub struct StateStore {
    state_dir: PathBuf,
    state_path: PathBuf,
    claims_dir: PathBuf,
    lock_path: PathBuf,
}

// Submodule declarations and re-exports. The public surface keeps
// version constants, model types, and `StateStore` at `crate::state::queue::*`.

pub mod claim;
pub mod daemon_lock;
pub mod reaper;
pub mod store;

use self::claim::*;
use self::daemon_lock::*;
use self::reaper::*;
use self::store::*;

pub use claim::*;
pub use daemon_lock::*;
pub use reaper::*;
pub use store::*;
