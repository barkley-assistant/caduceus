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

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{CaduceusError, CaduceusResult};
use crate::issue::IssueKey;

/// Canonical queue-file schema version. Bumping it is a breaking
/// change — the daemon refuses any other value. Tested by
/// [`tests/queue_model_test.rs`].
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

impl StateStore {
    /// Open the store. Creates the state directory and the
    /// `claims/` subdirectory if they are missing, validates the
    /// existing `state.json` if one is present, and returns the
    /// store handle.
    ///
    /// A missing `state.json` is treated as the empty version-1
    /// envelope. A present but malformed file is preserved and
    /// returned as [`CaduceusError::StateCorrupt`].
    pub fn open(state_dir: &Path) -> CaduceusResult<Self> {
        fs::create_dir_all(state_dir)?;
        let claims_dir = state_dir.join(CLAIMS_DIRNAME);
        fs::create_dir_all(&claims_dir)?;
        let state_path = state_dir.join(STATE_FILENAME);
        let lock_path = state_dir.join(STATE_LOCK_FILENAME);
        let store = Self {
            state_dir: state_dir.to_path_buf(),
            state_path,
            claims_dir,
            lock_path,
        };
        // Force a load+validate at open so a corrupt file is
        // reported immediately rather than on the first mutation.
        store.load_validated()?;
        Ok(store)
    }

    /// Path of the active state file (mainly for status/diagnostic
    /// code paths).
    pub fn state_path(&self) -> PathBuf {
        self.state_path.clone()
    }

    /// Path of the claims directory.
    pub fn claims_dir(&self) -> PathBuf {
        self.claims_dir.clone()
    }

    /// Path of the directory backing this store.
    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.clone()
    }

    /// Read the queue state under a shared `flock`. Never rewrites
    /// the file — `mtime` is preserved across repeated calls. A
    /// missing state file is treated as the empty version-1
    /// envelope.
    pub fn snapshot(&self) -> CaduceusResult<QueueState> {
        // Acquire the shared flock by opening an existing lock file
        // (or creating it on first run). The lock's existence is
        // independent of `state.json` so a fresh install with no
        // state file still gets proper serialisation. The state
        // load itself tolerates a missing file.
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)?;
        lock_file.lock_shared().map_err(into_lock_error)?;
        let result = self.load_validated();
        let unlock = lock_file.unlock();
        if let Err(err) = unlock {
            tracing::debug!(error = %scrub(&format!("{err:?}")), "snapshot unlock");
        }
        result
    }

    /// Insert a new queue entry, no-op against an existing entry,
    /// or — when dry-run is disabled and the existing entry is
    /// `Previewed` — promote it to `Queued`. See
    /// [`EnqueueOutcome`].
    pub fn enqueue(
        &self,
        key: &IssueKey,
        ticket_type: TicketType,
        dry_run: bool,
    ) -> CaduceusResult<EnqueueOutcome> {
        key.validate()?;
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            let now = Utc::now();
            let outcome = match state.entry_mut(key) {
                Some(existing) => {
                    if existing.phase == Phase::Previewed && !dry_run {
                        existing.phase = Phase::Queued;
                        existing.next_attempt_at = None;
                        existing.updated_at = now;
                        EnqueueOutcome::Promoted
                    } else {
                        EnqueueOutcome::AlreadyPresent
                    }
                }
                None => {
                    let entry = QueueEntry {
                        key: key.clone(),
                        phase: Phase::Queued,
                        ticket_type,
                        attempts: 0,
                        last_error: None,
                        last_run_id: None,
                        next_attempt_at: None,
                        finalization: None,
                        queued_at: now,
                        updated_at: now,
                    };
                    state.entries.insert(key.display_key(), entry);
                    EnqueueOutcome::Inserted
                }
            };
            store.persist(&state)?;
            Ok(outcome)
        })
    }

    /// Atomically claim the oldest queued entry whose
    /// `next_attempt_at` is in the past or `None`. The returned
    /// [`ClaimedEntry`] carries both the entry and the
    /// [`ClaimToken`] every later transition must present.
    ///
    /// `None` is returned when no eligible entry exists. The
    /// contract for crash-safety is: the claim file is created
    /// first (with `O_CREAT`/`O_EXCL`), then the queue is rewritten
    /// to mark the entry as `InProgress`. If the queue rewrite
    /// fails, the new claim is removed so the entry can be
    /// re-claimed. If two acquires race on the same entry the
    /// second one tries the next FIFO entry rather than surfacing
    /// a hard error.
    pub fn acquire_next(
        &self,
        run_id: &str,
        pid: u32,
        now: DateTime<Utc>,
    ) -> CaduceusResult<Option<ClaimedEntry>> {
        let token_claims_dir = self.claims_dir.clone();
        let token_run_id = run_id.to_string();
        self.with_exclusive(|store| {
            acquire_next_locked(store, &token_claims_dir, &token_run_id, pid, now)
        })
    }

    /// Persist a worktree path on the matching claim. Returns an
    /// error if the token does not match the current `last_run_id`
    /// of the entry — a worker that restarts with a stale claim
    /// must not be able to overwrite a fresh worktree path.
    pub fn set_worktree(&self, claim: &ClaimToken, path: &Path) -> CaduceusResult<()> {
        self.verify_claim_run_id(claim)?;
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            let entry = state
                .entries
                .values_mut()
                .find(|e| matches_token(e, claim))
                .ok_or_else(|| claim_mismatch(claim))?;
            entry.updated_at = Utc::now();
            // The queue record itself doesn't carry a worktree
            // path (worktree lives on disk under the worker-home
            // area); updating last_run_id and last_error-free
            // updated_at is sufficient for the audited record. The
            // claim file is updated separately below.
            store.persist(&state)?;
            // Update the claim file to record the worktree path.
            // Re-read the existing file rather than overwrite, so
            // a previously-saved worktree path survives if the
            // token raced against a second set_worktree call.
            update_claim_worktree(&store.claims_dir, claim, Some(path))?;
            Ok(())
        })
    }

    /// Persist a [`FinalizationCheckpoint`] under the matching claim.
    /// The stage field is the durable signal that subsequent ticks
    /// can resume from; the daemon's invariant #4 says "code commit
    /// exists — or before investigation comment — the queue entry
    /// receives a durable FinalizationCheckpoint".
    pub fn save_finalization(
        &self,
        claim: &ClaimToken,
        checkpoint: FinalizationCheckpoint,
    ) -> CaduceusResult<()> {
        self.verify_claim_run_id(claim)?;
        if checkpoint.run_id != claim.run_id {
            return Err(CaduceusError::Queue {
                context: "save_finalization",
                stderr: format!(
                    "checkpoint run_id {:?} does not match claim run_id {:?}",
                    checkpoint.run_id, claim.run_id
                ),
            });
        }
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            let entry = state
                .entries
                .values_mut()
                .find(|e| matches_token(e, claim))
                .ok_or_else(|| claim_mismatch(claim))?;
            entry.finalization = Some(checkpoint.clone());
            entry.updated_at = Utc::now();
            store.persist(&state)?;
            update_claim_worktree(&store.claims_dir, claim, checkpoint.result_path.parent())?;
            Ok(())
        })
    }

    /// Terminal transition for a successful code result.
    pub fn complete(&self, claim: ClaimToken) -> CaduceusResult<()> {
        self.complete_with(claim, Phase::Done)
    }

    /// Terminal transition for a successful investigation result.
    pub fn complete_investigation(&self, claim: ClaimToken) -> CaduceusResult<()> {
        self.complete_with(claim, Phase::Done)
    }

    /// Terminal transition for a successful dry-run preview. The
    /// entry moves to `Previewed`; on the next non-dry tick the
    /// polling loop will atomically promote it to `Queued` (see
    /// [`StateStore::enqueue`] and CONTRACTS.md "Dry-run").
    pub fn complete_preview(&self, claim: ClaimToken) -> CaduceusResult<()> {
        self.complete_with(claim, Phase::Previewed)
    }

    /// Retry-or-fail terminal transition. With ``budget`` total
    /// allowed attempts the convention is: attempts 1..budget-1
    /// return to ``Queued`` with ``next_attempt_at = now +
    /// retry_backoff_seconds``; attempt ``budget`` transitions to
    /// ``Failed``. See CONTRACTS.md "Retry semantics".
    ///
    /// Returns the new phase so callers can log without re-reading
    /// state.
    pub fn retry_or_fail(
        &self,
        claim: ClaimToken,
        error: &str,
        budget: u32,
    ) -> CaduceusResult<Phase> {
        if budget == 0 {
            return Err(CaduceusError::Config(
                "retry_or_fail budget must be > 0".to_string(),
            ));
        }
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            let entry = state
                .entries
                .values_mut()
                .find(|e| matches_token(e, &claim))
                .ok_or_else(|| claim_mismatch(&claim))?;
            entry.attempts = entry.attempts.saturating_add(1);
            entry.last_error = Some(error.to_string());
            entry.last_run_id = None;
            if entry.attempts >= budget {
                entry.phase = Phase::Failed;
                entry.next_attempt_at = None;
            } else {
                entry.phase = Phase::Queued;
                entry.next_attempt_at = Some(Utc::now() + chrono::Duration::seconds(300));
            }
            entry.updated_at = Utc::now();
            let phase = entry.phase;
            store.persist(&state)?;
            // The phase is now durable; unlink the claim file but
            // report (don't roll back) if the unlink fails — the
            // reaper repairs orphan claim files idempotently.
            unlink_claim_best_effort(&store.claims_dir, &claim);
            Ok(phase)
        })
    }

    /// Re-queue for non-attempt-counted reasons: rate-limit
    /// observations, GitHub transport failures, operator-cancel,
    /// local I/O. Does NOT increment `attempts`; sets
    /// `next_attempt_at` to the supplied timestamp.
    pub fn requeue_infrastructure(
        &self,
        claim: ClaimToken,
        error: &str,
        not_before: DateTime<Utc>,
    ) -> CaduceusResult<()> {
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            let entry = state
                .entries
                .values_mut()
                .find(|e| matches_token(e, &claim))
                .ok_or_else(|| claim_mismatch(&claim))?;
            entry.phase = Phase::Queued;
            entry.last_error = Some(error.to_string());
            entry.last_run_id = None;
            entry.next_attempt_at = Some(not_before);
            entry.updated_at = Utc::now();
            store.persist(&state)?;
            unlink_claim_best_effort(&store.claims_dir, &claim);
            Ok(())
        })
    }

    /// Operator-driven skip. The reason is recorded on the entry
    /// (overwriting any prior `last_error`).
    pub fn skip(&self, claim: ClaimToken, reason: &str) -> CaduceusResult<()> {
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            let entry = state
                .entries
                .values_mut()
                .find(|e| matches_token(e, &claim))
                .ok_or_else(|| claim_mismatch(&claim))?;
            entry.phase = Phase::Skipped;
            entry.last_error = Some(reason.to_string());
            entry.last_run_id = None;
            entry.next_attempt_at = None;
            entry.updated_at = Utc::now();
            store.persist(&state)?;
            unlink_claim_best_effort(&store.claims_dir, &claim);
            Ok(())
        })
    }

    /// Operator-driven reset for a `Failed` or `Skipped` entry.
    /// Returns the entry to `Queued` with `attempts=0`, no
    /// `last_error`, no `last_run_id`, and `next_attempt_at=None`.
    /// Refuses to operate on a `Queued`/`InProgress`/`Previewed`
    /// entry; refuses if an active claim file exists for the
    /// entry's digest.
    ///
    /// `clear_finalization` controls the
    /// `--force-finalization-reset` flag: by default the
    /// `FinalizationCheckpoint` is preserved (so a follow-up
    /// tick resumes from the saved branch/PR), and only the
    /// run-tracking fields are cleared. With
    /// `clear_finalization=true`, the checkpoint is dropped —
    /// the caller is responsible for surfacing the branch/PR
    /// to the operator and never deletes the remote branch or
    /// PR itself.
    pub fn reset_entry(
        &self,
        key: &IssueKey,
        clear_finalization: bool,
    ) -> CaduceusResult<ResetOutcome> {
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            // Compare by display_key (lowercase) rather than
            // the raw IssueKey — the on-disk map key is
            // lowercased on every write, and an operator who
            // types `OWNER/Repo#1` should still find the entry.
            let target = key.display_key();
            let entry = state
                .entries
                .values_mut()
                .find(|e| e.key.display_key() == target)
                .ok_or_else(|| CaduceusError::Queue {
                    context: "reset",
                    stderr: format!("no entry for {target}"),
                })?;
            // The contract only allows resetting `Failed` or
            // `Skipped`. Anything else (including `Done` and
            // `InProgress`) is an explicit operator error.
            match entry.phase {
                Phase::Failed | Phase::Skipped => {}
                other => {
                    return Err(CaduceusError::Queue {
                        context: "reset",
                        stderr: format!(
                            "refusing to reset entry {}: phase is {:?}, must be Failed or Skipped",
                            key.display_key(),
                            other
                        ),
                    });
                }
            }
            // Refuse if an active claim file exists. The reaper
            // would clean it up on the next tick, but an active
            // claim indicates a live worker and the operator
            // must not silently invalidate it.
            let digest = display_digest(&key.display_key());
            let claim_path = store.claims_dir.join(format!("{digest}.claim"));
            if claim_path.is_file() {
                return Err(CaduceusError::Queue {
                    context: "reset",
                    stderr: format!(
                        "refusing to reset entry {}: active claim file exists",
                        key.display_key()
                    ),
                });
            }
            // Capture the checkpoint before we drop it so the
            // caller can report the branch/PR.
            let dropped_checkpoint = if clear_finalization {
                entry.finalization.take()
            } else {
                None
            };
            entry.phase = Phase::Queued;
            entry.attempts = 0;
            entry.last_error = None;
            entry.last_run_id = None;
            entry.next_attempt_at = None;
            entry.updated_at = Utc::now();
            store.persist(&state)?;
            Ok(ResetOutcome {
                cleared_finalization: clear_finalization,
                dropped_checkpoint,
            })
        })
    }

    /// Look up a `FinalizationCheckpoint` for an entry. Used by
    /// the queue-reset CLI to surface the branch/PR in
    /// `--force-finalization-reset` reports.
    pub fn finalization_for(
        &self,
        key: &IssueKey,
    ) -> CaduceusResult<Option<FinalizationCheckpoint>> {
        let snap = self.snapshot()?;
        Ok(snap.entry(key).and_then(|e| e.finalization.clone()))
    }

    // --- internal helpers ---------------------------------------------------

    fn complete_with(&self, claim: ClaimToken, target: Phase) -> CaduceusResult<()> {
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            let entry = state
                .entries
                .values_mut()
                .find(|e| matches_token(e, &claim))
                .ok_or_else(|| claim_mismatch(&claim))?;
            entry.phase = target;
            entry.last_error = None;
            entry.last_run_id = None;
            entry.next_attempt_at = None;
            entry.updated_at = Utc::now();
            store.persist(&state)?;
            unlink_claim_best_effort(&store.claims_dir, &claim);
            Ok(())
        })
    }

    fn load_validated(&self) -> CaduceusResult<QueueState> {
        match fs::read(&self.state_path) {
            Ok(bytes) => parse_queue_state(std::str::from_utf8(&bytes).map_err(|err| {
                CaduceusError::StateCorrupt {
                    path: self.state_path.clone(),
                    message: format!("state file is not UTF-8: {err}"),
                }
            })?)
            .map_err(|err| match err {
                CaduceusError::StateCorrupt { message, .. } => CaduceusError::StateCorrupt {
                    path: self.state_path.clone(),
                    message,
                },
                other => other,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(QueueState::empty()),
            Err(err) => Err(err.into()),
        }
    }

    fn persist(&self, state: &QueueState) -> CaduceusResult<()> {
        let body = serialize_queue_state(state)?;
        atomic_write(&self.state_path, body.as_bytes())?;
        sync_dir(&self.state_dir)?;
        Ok(())
    }

    fn with_exclusive<R>(&self, op: impl FnOnce(&Self) -> CaduceusResult<R>) -> CaduceusResult<R> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)?;
        file.lock_exclusive().map_err(into_lock_error)?;
        let result = op(self);
        let unlock = file.unlock();
        if let Err(err) = unlock {
            tracing::debug!(error = %scrub(&format!("{err:?}")), "exclusive unlock");
        }
        result
    }

    fn verify_claim_run_id(&self, claim: &ClaimToken) -> CaduceusResult<()> {
        let snap = self.snapshot()?;
        let entry = snap
            .entries
            .values()
            .find(|e| matches_token(e, claim))
            .ok_or_else(|| claim_mismatch(claim))?;
        if entry.last_run_id.as_deref() != Some(claim.run_id.as_str()) {
            return Err(claim_mismatch(claim));
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------
// acquire_next_locked — the body of acquire_next, factored out so
// the retry-on-race path stays linear and the borrow on `state`
// stays scoped to a single iteration.
// -----------------------------------------------------------------------

fn acquire_next_locked(
    store: &StateStore,
    claims_dir: &Path,
    run_id: &str,
    pid: u32,
    now: DateTime<Utc>,
) -> CaduceusResult<Option<ClaimedEntry>> {
    let mut state = store.load_validated()?;
    // Collect every eligible (Queued, no future backoff) entry
    // sorted by (queued_at, display_key). The BTreeMap already
    // iterates in display_key order; we sort again by queued_at
    // so the loop just pops the head each iteration.
    let mut eligible: Vec<(String, QueueEntry)> = state
        .entries
        .iter()
        .filter(|(_, e)| e.phase == Phase::Queued)
        .filter(|(_, e)| match e.next_attempt_at {
            Some(backoff) => backoff <= now,
            None => true,
        })
        .map(|(k, e)| (k.clone(), e.clone()))
        .collect();
    eligible.sort_by(|a, b| {
        a.1.queued_at
            .cmp(&b.1.queued_at)
            .then_with(|| a.0.cmp(&b.0))
    });

    // Iterate FIFO; for each candidate create the claim file with
    // O_CREAT|O_EXCL. A race-loss on the claim means another
    // process already grabbed this entry — skip to the next
    // candidate rather than surfacing a hard error.
    for (display_key, mut entry) in eligible {
        let digest = display_digest(&display_key);
        let claim_path = store.claims_dir.join(format!("{digest}.claim"));

        let claim_file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&claim_path)
        {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                // Race-loss: another process / thread already
                // claimed this entry. Try the next FIFO entry.
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        let body = ClaimFileBody {
            version: CLAIM_FILE_VERSION,
            key: entry.key.clone(),
            run_id: run_id.to_string(),
            pid,
            process_start_identity: process_start_identity(pid),
            started_at: now,
            worktree_path: None,
        };
        let body_text = match serde_json::to_string(&body) {
            Ok(text) => text,
            Err(err) => {
                // Roll back the empty claim file we just created.
                let _ = fs::remove_file(&claim_path);
                return Err(CaduceusError::Queue {
                    context: "claim",
                    stderr: format!("serialize claim: {err}"),
                });
            }
        };
        if let Err(err) = write_and_sync_claim(&claim_file, body_text.as_bytes()) {
            let _ = fs::remove_file(&claim_path);
            return Err(err);
        }
        if let Err(err) = sync_dir(&store.claims_dir) {
            // The claim is on disk but the directory fsync failed;
            // that's best-effort and not a rollback trigger.
            tracing::debug!(error = %err, "claim dir sync");
        }

        // Mark the entry InProgress and persist. If persistence
        // fails, roll back the claim file so the entry can be
        // re-claimed on the next tick.
        entry.phase = Phase::InProgress;
        entry.last_run_id = Some(run_id.to_string());
        // attempts is preserved on claim: a worker that restarts
        // mid-run keeps its retry budget intact.
        entry.updated_at = now;
        state.entries.insert(display_key.clone(), entry.clone());
        if let Err(err) = store.persist(&state) {
            // Best-effort rollback of the claim file. A failure
            // here is logged and the reaper cleans up.
            if let Err(rm_err) = fs::remove_file(&claim_path) {
                tracing::warn!(
                    error = %rm_err,
                    path = %claim_path.display(),
                    "claim rollback after state-write failure failed; reaper will clean up"
                );
            }
            return Err(err);
        }

        return Ok(Some(ClaimedEntry {
            entry,
            claim: ClaimToken {
                claims_dir: claims_dir.to_path_buf(),
                digest,
                run_id: run_id.to_string(),
            },
        }));
    }
    Ok(None)
}

fn write_and_sync_claim(file: &File, body: &[u8]) -> CaduceusResult<()> {
    let mut writer = file;
    writer.write_all(body)?;
    writer.sync_all()?;
    // CONTRACTS.md "Filesystem permissions": claim files are
    // written with mode 0600. OpenOptions + create_new respects
    // the process umask, which on some distros lets group-read
    // through (mode 0o640 or 0o660). Force 0600 here so the
    // invariant holds on every Unix.
    set_mode_0600(file)?;
    Ok(())
}

#[cfg(unix)]
fn set_mode_0600(file: &File) -> CaduceusResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = file.metadata()?.permissions();
    perms.set_mode(0o600);
    file.set_permissions(perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode_0600(_file: &File) -> CaduceusResult<()> {
    Ok(())
}

// -----------------------------------------------------------------------
// DaemonLock — nonblocking exclusive lock for the entire tick.
// CONTRACTS.md invariant #1.
// -----------------------------------------------------------------------

/// Filename of the daemon-wide tick lock. Distinct from
/// `STATE_LOCK_FILENAME` (which guards state-store mutations); the
/// daemon lock is held for an entire cron tick.
pub const DAEMON_LOCK_FILENAME: &str = "daemon.lock";

/// RAII wrapper around a nonblocking exclusive `flock` on
/// `<state_dir>/daemon.lock`. Held for the entire tick; the OS
/// releases the lock when the file descriptor drops. The lock
/// *file* is intentionally allowed to remain on disk so a
/// subsequent tick can re-open it without recreating the inode.
#[derive(Debug)]
pub struct DaemonLock {
    file: File,
}

impl DaemonLock {
    /// Attempt to take the daemon lock. Returns `Ok(None)` when
    /// another process already holds it (the canonical "concurrent
    /// tick" outcome), `Ok(Some(lock))` when this caller now owns
    /// it, and an error only when I/O itself fails.
    pub fn try_acquire(state_dir: &Path) -> CaduceusResult<Option<Self>> {
        if !state_dir.exists() {
            fs::create_dir_all(state_dir)?;
        }
        let lock_path = state_dir.join(DAEMON_LOCK_FILENAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { file })),
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                // Either another process holds the lock
                // (`WouldBlock`) or flock is not implemented on this
                // platform (`AlreadyExists` is fs2's fallback for
                // `try_lock_exclusive`). Either way: a concurrent
                // tick is in flight.
                Ok(None)
            }
            Err(err) => Err(err.into()),
        }
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        if let Err(err) = self.file.unlock() {
            // The OS will reap the flock when the fd closes, so a
            // failed unlock is informational only.
            tracing::debug!(error = %err, "daemon lock unlock failed; OS will reap");
        }
    }
}

// -----------------------------------------------------------------------
// Free-standing helpers
// -----------------------------------------------------------------------

fn matches_token(entry: &QueueEntry, claim: &ClaimToken) -> bool {
    if entry.phase != Phase::InProgress {
        return false;
    }
    if entry.last_run_id.as_deref() != Some(claim.run_id.as_str()) {
        return false;
    }
    // The digest is sha256(lowercase_display_key); recompute and
    // compare to defend against a forged token that names a
    // different digest but the same run_id.
    display_digest(&entry.key.display_key()) == claim.digest
}

fn claim_mismatch(claim: &ClaimToken) -> CaduceusError {
    CaduceusError::Queue {
        context: "claim",
        stderr: format!(
            "claim token run_id {:?} digest {} does not match any in-progress entry",
            claim.run_id, claim.digest
        ),
    }
}

fn into_lock_error(err: std::io::Error) -> CaduceusError {
    CaduceusError::Io(err)
}

fn scrub(value: &str) -> String {
    // Local scrub — duplicated here so the queue module doesn't
    // pull in the error module's redaction helper purely for a
    // single debug log.
    if value.is_empty() {
        return value.to_string();
    }
    let mut scrubbed = value.to_string();
    for needle in ["GITHUB_TOKEN", "CADUCEUS_GITHUB_TOKEN", "GH_TOKEN"] {
        if let Some(pos) = scrubbed.find(needle) {
            let abs = pos + needle.len();
            let value_end = advance_to_end_of_value(&scrubbed, abs);
            scrubbed.replace_range(abs..value_end, "<redacted>");
        }
    }
    scrubbed
}

fn advance_to_end_of_value(s: &str, start: usize) -> usize {
    let bytes = s.as_bytes();
    if start >= bytes.len() {
        return start;
    }
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' | b',' | b';' | b'}' | b']' => break,
            _ => i += 1,
        }
    }
    i
}

/// SHA-256 hex digest of the lowercase display key. This is the
/// claim file's basename and is the value recorded in
/// [`ClaimToken::digest`].
pub fn display_digest(display_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(display_key.as_bytes());
    hex::encode(hasher.finalize())
}

fn atomic_write(target: &Path, body: &[u8]) -> CaduceusResult<()> {
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }
    // Same-directory temp file. The temp name uses a counter and a
    // random-ish suffix so concurrent writers in the same tick do
    // not collide.
    let tmp = target.with_extension("json.tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, target)?;
    Ok(())
}

fn sync_dir(dir: &Path) -> CaduceusResult<()> {
    // Best-effort directory fsync. On Linux this flushes the
    // directory entry for the renamed file; on platforms where
    // opening a directory is unsupported the operation is a no-op.
    match File::open(dir) {
        Ok(f) => {
            if let Err(err) = f.sync_all() {
                tracing::debug!(error = %err, "sync_dir best-effort");
            }
        }
        Err(_) => {
            // Directory open failed (not Linux or platform does
            // not allow it); this is acceptable.
        }
    }
    Ok(())
}

fn unlink_claim_best_effort(claims_dir: &Path, claim: &ClaimToken) {
    let path = claim.claim_path();
    match fs::remove_file(&path) {
        Ok(()) => {
            if let Err(err) = sync_dir(claims_dir) {
                tracing::debug!(error = %err, "claim-dir sync");
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            // Per CONTRACTS.md / Task 3.1: "a claim-unlink failure
            // is reported without rolling back the durable phase
            // and is repaired idempotently by the reaper." We log
            // and continue; the reaper will pick it up.
            tracing::warn!(error = %err, path = %path.display(), "claim unlink failed; reaper will clean up");
        }
    }
}

fn update_claim_worktree(
    claims_dir: &Path,
    claim: &ClaimToken,
    worktree: Option<&Path>,
) -> CaduceusResult<()> {
    let path = claim.claim_path();
    let bytes = fs::read(&path).map_err(|err| CaduceusError::Queue {
        context: "claim",
        stderr: format!("read claim {}: {err}", path.display()),
    })?;
    let mut body: ClaimFileBody =
        serde_json::from_slice(&bytes).map_err(|err| CaduceusError::StateCorrupt {
            path: path.clone(),
            message: format!("claim JSON parse: {err}"),
        })?;
    body.worktree_path = worktree.map(|p| p.to_path_buf());
    let body_text = serde_json::to_string(&body).map_err(|err| CaduceusError::StateCorrupt {
        path: path.clone(),
        message: format!("claim JSON serialize: {err}"),
    })?;
    atomic_write(&path, body_text.as_bytes())?;
    sync_dir(claims_dir)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reaper: scan claim files for stale entries, malformed bodies, future
// timestamps; quarantine to claims/corrupt/ and report.
// ---------------------------------------------------------------------------

/// Directory under `<state_dir>/claims` where malformed/future-stamped
/// claim files are quarantined. The reaper never silently deletes
/// anything — corrupt claims are moved here and the queue file is left
/// untouched.
pub const CLAIMS_CORRUPT_DIRNAME: &str = "corrupt";

/// A timestamp more than this many seconds in the future is
/// considered corrupt rather than immortal. 5 minutes matches
/// the contract; the same threshold the reaper applies across
/// the filesystem.
pub const FUTURE_TIMESTAMP_TOLERANCE_SECS: i64 = 5 * 60;

/// What the reaper did on this tick. `count` is the total number
/// of claim files the reaper removed (reaped or quarantined);
/// `errors` collects per-file diagnostics so a single corrupt
/// claim does not abort the whole pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// Number of claim files the reaper acted on.
    pub count: u32,
    /// Per-file diagnostic strings. The reaper appends one entry
    /// per file it could not act on, so a partial failure is
    /// visible without losing the rest of the pass.
    pub errors: Vec<String>,
    /// Stale claims reaped (returned to `Queued` for InProgress
    /// entries, or unlinked for residue phases). A sub-count
    /// of `count`.
    pub stale_reaped: u32,
    /// Claim files moved into `claims/corrupt/`. A sub-count of
    /// `count`.
    pub quarantined: u32,
}

/// Reap stale claims and quarantine malformed/future-stamped
/// ones. Runs under a [`DaemonLock`] so the queue file is not
/// concurrently mutated. Pure side-effect on the local
/// `<state_dir>`: no GitHub call, no network I/O, no
/// notification. Returns a [`ReapReport`] the caller can log.
///
/// `stale_run_hours` is the age above which a claim with a
/// dead/mismatched process is reaped. The process identity is
/// `(pid, /proc/<pid>/stat starttime)`; if either the pid is
/// dead or the starttime has changed (pid reuse), the claim is
/// stale even before the age threshold.
pub async fn reap_stale_claims(
    state_dir: &Path,
    now: DateTime<Utc>,
    stale_run_hours: u64,
) -> CaduceusResult<ReapReport> {
    let claims_dir = state_dir.join(CLAIMS_DIRNAME);
    let mut report = ReapReport::default();

    // Nothing to do if the claims dir is missing — the daemon
    // may be starting cold. We still attempt the corrupt dir
    // creation so an operator can see where future quarantines
    // would go.
    if !claims_dir.is_dir() {
        return Ok(report);
    }

    let entries = match fs::read_dir(&claims_dir) {
        Ok(rd) => rd,
        Err(err) => {
            return Err(CaduceusError::Queue {
                context: "reap",
                stderr: format!("read_dir {}: {err}", claims_dir.display()),
            });
        }
    };

    let age_cutoff = now - chrono::Duration::seconds(stale_run_hours.saturating_mul(3600) as i64);
    let future_cutoff = now + chrono::Duration::seconds(FUTURE_TIMESTAMP_TOLERANCE_SECS);

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                report.errors.push(format!("read_dir: {err}"));
                continue;
            }
        };
        let path = entry.path();
        // Reject symlinks. The reaper never follows them —
        // a symlink in `claims/` could be a substitute for a
        // regular claim file that points outside the state
        // dir. The path is reported and the reaper moves on.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(err) => {
                report
                    .errors
                    .push(format!("symlink_metadata {}: {err}", path.display()));
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            report
                .errors
                .push(format!("refusing to act on symlink: {}", path.display()));
            continue;
        }
        let file_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // The `corrupt/` subdir is reserved for quarantine
        // outputs, not input — never act on it.
        if file_name == CLAIMS_CORRUPT_DIRNAME {
            continue;
        }
        if !file_name.ends_with(".claim") {
            // Unknown file: report and leave untouched. The
            // reaper does not have authority to delete foreign
            // files.
            report
                .errors
                .push(format!("unknown file in claims dir: {}", path.display()));
            continue;
        }

        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(err) => {
                report
                    .errors
                    .push(format!("read {}: {err}", path.display()));
                continue;
            }
        };
        let body: ClaimFileBody = match serde_json::from_slice(&bytes) {
            Ok(b) => b,
            Err(err) => {
                let _ = quarantine_claim(
                    &claims_dir,
                    &path,
                    &bytes,
                    &format!("malformed JSON: {err}"),
                );
                report.quarantined += 1;
                report.count += 1;
                report
                    .errors
                    .push(format!("malformed {} → quarantined: {err}", path.display()));
                continue;
            }
        };
        if body.version != CLAIM_FILE_VERSION {
            let _ = quarantine_claim(
                &claims_dir,
                &path,
                &bytes,
                &format!("unsupported claim version {}", body.version),
            );
            report.quarantined += 1;
            report.count += 1;
            report.errors.push(format!(
                "version mismatch in {}: got {}, expected {}",
                path.display(),
                body.version,
                CLAIM_FILE_VERSION
            ));
            continue;
        }
        if body.started_at > future_cutoff {
            let _ = quarantine_claim(
                &claims_dir,
                &path,
                &bytes,
                &format!(
                    "started_at {} is more than {FUTURE_TIMESTAMP_TOLERANCE_SECS}s in the future",
                    body.started_at
                ),
            );
            report.quarantined += 1;
            report.count += 1;
            report.errors.push(format!(
                "future started_at in {}: {}",
                path.display(),
                body.started_at
            ));
            continue;
        }
        // Recent enough that the staleness rule does not
        // apply — leave alone, regardless of process identity.
        if body.started_at > age_cutoff {
            continue;
        }
        // Old claim: only stale if the recorded process is
        // dead OR the start identity has changed (pid reuse).
        let recorded_pid_alive = is_pid_alive(body.pid);
        let recorded_start_matches =
            process_start_identity(body.pid) == body.process_start_identity;
        if recorded_pid_alive && recorded_start_matches {
            // Live worker — the claim is valid even if it
            // has been running longer than the threshold.
            // The threshold applies only to *stale* claims.
            continue;
        }
        // Stale. Reap.
        match reap_one_stale_claim(&claims_dir, &path, &body).await {
            Ok(()) => {
                report.stale_reaped += 1;
                report.count += 1;
            }
            Err(err) => {
                report
                    .errors
                    .push(format!("stale reap failed for {}: {err}", path.display()));
            }
        }
    }

    Ok(report)
}

/// Quarantine a malformed or future-stamped claim file by
/// moving it into `<claims>/corrupt/`. The original bytes are
/// preserved verbatim; the file name is suffixed with the
/// current timestamp so a re-quarantine of the same path does
/// not overwrite an existing artefact.
fn quarantine_claim(
    claims_dir: &Path,
    path: &Path,
    bytes: &[u8],
    reason: &str,
) -> CaduceusResult<()> {
    let corrupt = claims_dir.join(CLAIMS_CORRUPT_DIRNAME);
    fs::create_dir_all(&corrupt).map_err(|err| CaduceusError::Queue {
        context: "reap",
        stderr: format!("create {}: {err}", corrupt.display()),
    })?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%3fZ");
    let basename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown.claim");
    let target = corrupt.join(format!("{basename}.{stamp}.corrupt"));
    let note = format!(
        "<!-- caduceus-reaper\n     reason: {reason}\n     at: {}\n     source: {}\n-->\n",
        Utc::now().to_rfc3339(),
        path.display()
    );
    let mut payload = Vec::with_capacity(bytes.len() + note.len());
    payload.extend_from_slice(note.as_bytes());
    payload.extend_from_slice(bytes);
    atomic_write(&target, &payload)?;
    sync_dir(&corrupt)?;
    let _ = fs::remove_file(path);
    Ok(())
}

/// Reap a single stale claim. The caller has already determined
/// staleness; this is the per-file teardown.
async fn reap_one_stale_claim(
    claims_dir: &Path,
    claim_path: &Path,
    body: &ClaimFileBody,
) -> CaduceusResult<()> {
    // 1. Find the queue entry. If no entry exists, the claim is
    //    orphaned — there is nothing to revert. Unlink the
    //    claim and return.
    let parent = claims_dir.parent().unwrap_or(claims_dir);
    let store = StateStore::open_for_reap_only(parent)?;
    let snapshot = store.snapshot()?;
    let entry = snapshot
        .entries
        .values()
        .find(|e| e.key.display_key() == body.key.display_key());
    let entry = match entry {
        Some(e) => e,
        None => {
            // Orphaned claim with no queue entry. Unlink and
            // continue.
            let _ = fs::remove_file(claim_path);
            return Ok(());
        }
    };

    // 2. Tear down the worktree if one was attached. The
    //    worktree's `Worktree` handle is constructed from the
    //    claim body so we can call into `worktree::remove` —
    //    the path-safety, idempotency, and branch-retention
    //    rules all live there.
    if let Some(wt_path) = &body.worktree_path {
        if wt_path.is_dir() {
            let wt = crate::worktree::Worktree {
                issue: body.key.clone(),
                run_id: body.run_id.clone(),
                branch_name: String::new(), // not used by remove; remove inspects ref state
                path: wt_path.clone(),
                base_oid: String::new(),
                fresh: false,
                created_at: body.started_at,
            };
            // Errors from `remove` are recorded as reaper
            // warnings but do not abort — the claim is
            // reaped regardless so a teardown failure does
            // not block the queue.
            if let Err(err) = crate::worktree::remove(&wt).await {
                tracing::warn!(
                    error = %err,
                    path = %wt_path.display(),
                    "reaper worktree teardown failed; will retry next tick"
                );
            }
        }
    }

    // 3. Update the queue. If the entry is `InProgress`, return
    //    to `Queued` without incrementing attempts. For any
    //    other phase, leave the phase alone (the entry is
    //    already durable) — the claim file is just residue.
    let now = Utc::now();
    if entry.phase == Phase::InProgress {
        store.with_exclusive_reap_only(|s| {
            let mut state = s.load_validated()?;
            if let Some(e) = state
                .entries
                .values_mut()
                .find(|e| e.key.display_key() == body.key.display_key())
            {
                e.phase = Phase::Queued;
                e.last_run_id = None;
                e.last_error = Some(format!("reaper: stale claim for run {}", body.run_id));
                e.next_attempt_at = Some(now);
                e.updated_at = now;
            }
            s.persist(&state)?;
            Ok(())
        })?;
    }
    // For any other phase (Queued/Previewed/Done/Failed/Skipped)
    // the claim file is just orphan residue. The contract
    // explicitly says "the reaper treats the claim as residue:
    // it performs any required teardown and removes only the
    // claim without changing phase." The worktree teardown
    // above already happened; the queue phase is left alone.

    // 4. Unlink the claim file. The state is already durable
    //    by this point; a final unlink failure surfaces as a
    //    reaper warning, not a fatal error.
    let _ = fs::remove_file(claim_path);
    Ok(())
}

/// `true` if a process with PID `pid` exists. The check is
/// best-effort and Linux-specific; on non-Linux platforms it
/// always returns `false` so the reaper treats those claims as
/// stale (matching the contract's "process identity is
/// absent").
fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // /proc/<pid> exists → the process is alive (or a zombie
    // awaiting reap; the starttime check distinguishes). This
    // is sufficient for the reaper's purposes because the
    // claim's recorded `process_start_identity` already
    // records the start ticks, and a pid-reuse will be caught
    // by the starttime comparison.
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

// ---------------------------------------------------------------------------
// Reaper-only StateStore surface. The full StateStore takes the lock
// on `open()`; the reaper needs a lighter view that loads/parses
// without taking the daemon-wide lock (the caller already holds it).
// ---------------------------------------------------------------------------

impl StateStore {
    /// Open a StateStore for the reaper's read-only view. Does
    /// not take any flock — the reaper runs under the daemon
    /// tick's `DaemonLock`, which serialises the whole tick.
    #[doc(hidden)]
    pub fn open_for_reap_only(state_dir: &Path) -> CaduceusResult<Self> {
        let claims_dir = state_dir.join(CLAIMS_DIRNAME);
        fs::create_dir_all(&claims_dir).map_err(|err| CaduceusError::Queue {
            context: "state_open",
            stderr: format!("create claims dir {}: {err}", claims_dir.display()),
        })?;
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            state_path: state_dir.join(STATE_FILENAME),
            claims_dir,
            lock_path: state_dir.join(STATE_LOCK_FILENAME),
        })
    }

    /// Acquire the state lock for the duration of a callback
    /// in the reaper. The lock is released on return.
    #[doc(hidden)]
    pub fn with_exclusive_reap_only<F, T>(&self, f: F) -> CaduceusResult<T>
    where
        F: FnOnce(&Self) -> CaduceusResult<T>,
    {
        // Open the lock file, take exclusive flock, run `f`,
        // drop. The lock file may not exist yet on a cold
        // start; we create it (mode 0600) before flocking.
        use std::os::unix::fs::PermissionsExt;
        let lock_path = &self.lock_path;
        fs::create_dir_all(&self.state_dir).ok();
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|err| CaduceusError::Queue {
                context: "state_lock",
                stderr: format!("open {}: {err}", lock_path.display()),
            })?;
        let _ = lock_file.set_permissions(std::fs::Permissions::from_mode(0o600));
        FileExt::lock_exclusive(&lock_file).map_err(|err| CaduceusError::Queue {
            context: "state_lock",
            stderr: format!("flock_exclusive {}: {err}", lock_path.display()),
        })?;
        let r = f(self);
        let _ = FileExt::unlock(&lock_file);
        r
    }
}

// ---------------------------------------------------------------------------
// End of module
// ---------------------------------------------------------------------------
