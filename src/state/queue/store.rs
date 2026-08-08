#![allow(dead_code, unused_imports)]
use super::*;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::github::issue::IssueKey;
use crate::infra::error::{scrub, CaduceusError, CaduceusResult};

thread_local! {
    /// Thread-local transaction connection used by SQLite-backed
    /// [`StateStore::with_exclusive`]. The connection is opened at
    /// the start of the exclusive section, stored here so that inner
    /// calls to [`load_validated_sqlite`] and [`persist_sqlite`] can
    /// participate in the same `BEGIN IMMEDIATE` transaction, and
    /// removed once the section finishes.
    static SQLITE_TX_CONN: RefCell<Option<Connection>> = const { RefCell::new(None) };
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
            claims_dir,
            backend: StateStoreBackend::Json {
                state_path: state_path.clone(),
                lock_path: lock_path.clone(),
                state_dir: state_dir.to_path_buf(),
            },
        };
        // Force a load+validate at open so a corrupt file is
        // reported immediately rather than on the first mutation.
        store.load_validated()?;
        Ok(store)
    }

    /// Open a SQLite-backed store. Creates the state directory and
    /// the `claims/` subdirectory if they are missing and ensures
    /// the database schema is present. Claim files remain on disk for
    /// both backends per the crash-safety contract.
    pub fn open_sqlite(state_dir: &Path) -> CaduceusResult<Self> {
        fs::create_dir_all(state_dir)?;
        let claims_dir = state_dir.join(CLAIMS_DIRNAME);
        fs::create_dir_all(&claims_dir)?;
        // Open the database once to initialize the schema; further
        // operations open their own short-lived connections so the
        // store remains `Send`/`Sync` and can be shared across async
        // tasks via `Arc`.
        let _conn = crate::state::store::open_in(state_dir)?;
        let store = Self {
            state_dir: state_dir.to_path_buf(),
            claims_dir,
            backend: StateStoreBackend::Sqlite(state_dir.to_path_buf()),
        };
        // Like the JSON path, force a load so a corrupt/unknown
        // schema is reported at open time.
        let _ = store.load_validated()?;
        Ok(store)
    }

    /// Path of the active state file (mainly for status/diagnostic
    /// code paths).
    pub fn state_path(&self) -> PathBuf {
        match &self.backend {
            StateStoreBackend::Json { state_path, .. } => state_path.clone(),
            StateStoreBackend::Sqlite(state_dir) => {
                state_dir.join(crate::state::store::DB_FILENAME)
            }
        }
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
        match &self.backend {
            StateStoreBackend::Json { lock_path, .. } => {
                let lock_file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(lock_path)?;
                lock_file.lock_shared().map_err(into_lock_error)?;
                let result = self.load_validated();
                let unlock = lock_file.unlock();
                if let Err(err) = unlock {
                    tracing::debug!(error = %scrub(&format!("{err:?}")), "snapshot unlock");
                }
                result
            }
            StateStoreBackend::Sqlite(state_dir) => {
                // If we are already inside an exclusive section, reuse
                // its IMMEDIATE transaction. Otherwise open a fresh
                // connection for a read-only snapshot.
                SQLITE_TX_CONN.with(|cell| {
                    if let Some(conn) = cell.borrow().as_ref() {
                        load_validated_sqlite(conn, state_dir)
                    } else {
                        let conn = crate::state::store::open_in(state_dir)?;
                        load_validated_sqlite(&conn, state_dir)
                    }
                })
            }
        }
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
                        generation: 1,
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
        self.save_finalization_verified(claim, checkpoint)
    }

    /// Persist a checkpoint found during recovery under the currently
    /// valid claim. The checkpoint run ID intentionally remains the
    /// original run ID so future SQLite/result lookups use the durable
    /// recovery cursor rather than the fresh claim ID.
    pub fn save_resumed_finalization(
        &self,
        claim: &ClaimToken,
        checkpoint: FinalizationCheckpoint,
    ) -> CaduceusResult<()> {
        self.verify_claim_run_id(claim)?;
        self.save_finalization_verified(claim, checkpoint)
    }

    fn save_finalization_verified(
        &self,
        claim: &ClaimToken,
        checkpoint: FinalizationCheckpoint,
    ) -> CaduceusResult<()> {
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
    /// [`StateStore::enqueue`]).
    pub fn complete_preview(&self, claim: ClaimToken) -> CaduceusResult<()> {
        self.complete_with(claim, Phase::Previewed)
    }

    /// Transition an entry to AwaitingReview phase (key-based).
    ///
    /// The daemon calls this after posting the completion comment
    /// on an issue, but before any Done/close transition. The
    /// entry stays visible for polling by
    /// [`crate::daemon::tick::poll_awaiting_review_entries`] which
    /// checks the PR merge status on subsequent ticks.
    ///
    /// This method is intentionally key-based (not claim-based)
    /// because the entry may not be InProgress when the transition
    /// is applied — the claim lifecycle has already released the
    /// worker's worktree.
    pub fn complete_awaiting_review(&self, key: &IssueKey) -> CaduceusResult<()> {
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            let target = key.display_key();
            let entry = state
                .entries
                .values_mut()
                .find(|e| e.key.display_key() == target)
                .ok_or_else(|| CaduceusError::Queue {
                    context: "complete_awaiting_review",
                    stderr: format!("no entry for {target}"),
                })?;
            entry.phase = Phase::AwaitingReview;
            entry.updated_at = Utc::now();
            store.persist(&state)?;
            Ok(())
        })
    }

    /// Route an AwaitingReview entry to NeedsAttention with a
    /// diagnostic reason (e.g. PR was closed without merging).
    pub fn route_to_needs_attention(&self, key: &IssueKey, reason: &str) -> CaduceusResult<()> {
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            let target = key.display_key();
            let entry = state
                .entries
                .values_mut()
                .find(|e| e.key.display_key() == target)
                .ok_or_else(|| CaduceusError::Queue {
                    context: "route_to_needs_attention",
                    stderr: format!("no entry for {target}"),
                })?;
            if entry.phase != Phase::AwaitingReview {
                return Err(CaduceusError::Queue {
                    context: "route_to_needs_attention",
                    stderr: format!(
                        "entry {target} is {:?}, must be AwaitingReview",
                        entry.phase
                    ),
                });
            }
            entry.phase = Phase::NeedsAttention;
            entry.last_error = Some(reason.to_string());
            entry.updated_at = Utc::now();
            store.persist(&state)?;
            Ok(())
        })
    }

    /// Transition an AwaitingReview entry to Done because the PR
    /// was merged. Only succeeds when the current phase is
    /// [`Phase::AwaitingReview`]; returns an error otherwise.
    pub fn resolve_awaiting_review_as_done(&self, key: &IssueKey) -> CaduceusResult<()> {
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            let target = key.display_key();
            let entry = state
                .entries
                .values_mut()
                .find(|e| e.key.display_key() == target)
                .ok_or_else(|| CaduceusError::Queue {
                    context: "resolve_awaiting_review_as_done",
                    stderr: format!("no entry for {target}"),
                })?;
            if entry.phase != Phase::AwaitingReview {
                return Err(CaduceusError::Queue {
                    context: "resolve_awaiting_review_as_done",
                    stderr: format!(
                        "entry {target} is {:?}, must be AwaitingReview",
                        entry.phase
                    ),
                });
            }
            entry.phase = Phase::Done;
            entry.updated_at = Utc::now();
            store.persist(&state)?;
            Ok(())
        })
    }

    /// Retry-or-fail terminal transition. With ``budget`` total
    /// allowed attempts the convention is: attempts 1..budget-1
    /// return to ``Queued`` with ``next_attempt_at = now +
    /// retry_backoff_seconds``; attempt ``budget`` transitions to
    /// ``Failed``.
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
                Phase::Failed | Phase::Skipped | Phase::NeedsAttention => {}
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

    /// Create a new generation for an entry: increment generation,
    /// reset phase to `Queued`, and clear run-tracking fields.
    pub fn reprocess_entry(&self, key: &IssueKey) -> CaduceusResult<()> {
        self.with_exclusive(|store| {
            let mut state = store.load_validated()?;
            let target = key.display_key();
            let entry = state
                .entries
                .values_mut()
                .find(|e| e.key.display_key() == target)
                .ok_or_else(|| CaduceusError::Queue {
                    context: "reprocess",
                    stderr: format!("no entry for {target}"),
                })?;
            // Refuse to reprocess entries awaiting human review — the
            // operator must inspect and resolve the PR status first.
            if entry.phase == Phase::AwaitingReview {
                return Err(CaduceusError::Queue {
                    context: "reprocess",
                    stderr: format!(
                        "refusing to reprocess entry {target}: phase is AwaitingReview, \
  human review must complete first"
                    ),
                });
            }
            entry.phase = Phase::Queued;
            entry.attempts = 0;
            entry.last_error = None;
            entry.last_run_id = None;
            entry.next_attempt_at = None;
            entry.generation = entry.generation.saturating_add(1);
            entry.updated_at = Utc::now();
            store.persist(&state)?;
            Ok(())
        })
    }

    // --- internal helpers ---------------------------------------------------

    pub(crate) fn complete_with(&self, claim: ClaimToken, target: Phase) -> CaduceusResult<()> {
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

    pub(crate) fn load_validated(&self) -> CaduceusResult<QueueState> {
        match &self.backend {
            StateStoreBackend::Json { state_path, .. } => load_validated_json(state_path),
            StateStoreBackend::Sqlite(state_dir) => SQLITE_TX_CONN.with(|cell| {
                if let Some(conn) = cell.borrow().as_ref() {
                    load_validated_sqlite(conn, state_dir)
                } else {
                    let conn = crate::state::store::open_in(state_dir)?;
                    load_validated_sqlite(&conn, state_dir)
                }
            }),
        }
    }

    pub(crate) fn persist(&self, state: &QueueState) -> CaduceusResult<()> {
        match &self.backend {
            StateStoreBackend::Json {
                state_path,
                state_dir,
                ..
            } => {
                let body = serialize_queue_state(state)?;
                atomic_write(state_path, body.as_bytes())?;
                sync_dir(state_dir)
            }
            StateStoreBackend::Sqlite(state_dir) => SQLITE_TX_CONN.with(|cell| {
                if let Some(conn) = cell.borrow().as_ref() {
                    persist_sqlite(conn, state)
                } else {
                    let conn = crate::state::store::open_in(state_dir)?;
                    persist_sqlite(&conn, state)
                }
            }),
        }
    }

    pub(crate) fn with_exclusive<R>(
        &self,
        op: impl FnOnce(&Self) -> CaduceusResult<R>,
    ) -> CaduceusResult<R> {
        match &self.backend {
            StateStoreBackend::Json { lock_path, .. } => {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(lock_path)?;
                file.lock_exclusive().map_err(into_lock_error)?;
                let result = op(self);
                let unlock = file.unlock();
                if let Err(err) = unlock {
                    tracing::debug!(error = %scrub(&format!("{err:?}")), "exclusive unlock");
                }
                result
            }
            StateStoreBackend::Sqlite(state_dir) => {
                // Open one connection, start an IMMEDIATE transaction,
                // and make inner `load_validated`/`persist` calls
                // participate in that transaction via the thread-local.
                let conn = crate::state::store::open_in(state_dir)?;
                conn.execute("BEGIN IMMEDIATE", [])
                    .map_err(|e| CaduceusError::StateCorrupt {
                        path: state_dir.join(crate::state::store::DB_FILENAME),
                        message: format!("cannot begin IMMEDIATE transaction: {e}"),
                    })?;
                SQLITE_TX_CONN.with(|cell| {
                    *cell.borrow_mut() = Some(conn);
                });
                let result = op(self);
                let commit_error: Option<CaduceusError> = SQLITE_TX_CONN.with(|cell| {
                    cell.borrow_mut().take().and_then(|conn| {
                        if result.is_ok() {
                            match conn.execute("COMMIT", []) {
                                Ok(_) => None,
                                Err(e) => Some(CaduceusError::StateCorrupt {
                                    path: state_dir.join(crate::state::store::DB_FILENAME),
                                    message: format!("cannot commit exclusive transaction: {e}"),
                                }),
                            }
                        } else {
                            let _ = conn.execute("ROLLBACK", []);
                            None
                        }
                    })
                });
                if let Some(err) = commit_error {
                    return Err(err);
                }
                result
            }
        }
    }

    pub(crate) fn verify_claim_run_id(&self, claim: &ClaimToken) -> CaduceusResult<()> {
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

fn load_validated_json(state_path: &Path) -> CaduceusResult<QueueState> {
    match fs::read(state_path) {
        Ok(bytes) => parse_queue_state(std::str::from_utf8(&bytes).map_err(|err| {
            CaduceusError::StateCorrupt {
                path: state_path.to_path_buf(),
                message: format!("state file is not UTF-8: {err}"),
            }
        })?)
        .map_err(|err| match err {
            CaduceusError::StateCorrupt { message, .. } => CaduceusError::StateCorrupt {
                path: state_path.to_path_buf(),
                message,
            },
            other => other,
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(QueueState::empty()),
        Err(err) => Err(err.into()),
    }
}

fn phase_from_str(s: &str) -> Option<Phase> {
    serde_json::from_str::<Phase>(&format!("\"{s}\"")).ok()
}

fn ticket_type_from_str(s: &str) -> Option<TicketType> {
    serde_json::from_str::<TicketType>(&format!("\"{s}\"")).ok()
}

fn load_validated_sqlite(conn: &Connection, state_dir: &Path) -> CaduceusResult<QueueState> {
    let db_path = state_dir.join(crate::state::store::DB_FILENAME);
    let mut stmt = conn
        .prepare(
            "SELECT issue_key, phase, ticket_type, attempts, last_error, last_run_id,
                    next_attempt_at, finalization, queued_at, updated_at, generation
             FROM queue_entries",
        )
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot prepare queue_entries select: {e}"),
        })?;
    let rows = stmt
        .query_map([], row_to_entry)
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot read queue_entries: {e}"),
        })?;
    let mut entries = BTreeMap::new();
    for row in rows {
        let entry = row.map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot decode queue entry: {e}"),
        })?;
        entries.insert(entry.key.display_key(), entry);
    }
    Ok(QueueState {
        version: QUEUE_FILE_VERSION,
        entries,
    })
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<QueueEntry> {
    let issue_key: String = row.get(0)?;
    let phase_str: String = row.get(1)?;
    let ticket_type_str: String = row.get(2)?;
    let attempts: i64 = row.get(3)?;
    let last_error: Option<String> = row.get(4)?;
    let last_run_id: Option<String> = row.get(5)?;
    let next_attempt_at: Option<String> = row.get(6)?;
    let finalization: Option<String> = row.get(7)?;
    let queued_at: String = row.get(8)?;
    let updated_at: String = row.get(9)?;
    let generation: i64 = row.get(10)?;

    let key = IssueKey::parse(&issue_key).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid issue_key {issue_key}: {err:?}"),
            )),
        )
    })?;
    let phase = phase_from_str(&phase_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid phase {phase_str}"),
            )),
        )
    })?;
    let ticket_type = ticket_type_from_str(&ticket_type_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid ticket_type {ticket_type_str}"),
            )),
        )
    })?;

    Ok(QueueEntry {
        key,
        phase,
        ticket_type,
        attempts: attempts.max(0) as u32,
        last_error,
        last_run_id,
        next_attempt_at: next_attempt_at
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        finalization: finalization.and_then(|s| serde_json::from_str(&s).ok()),
        queued_at: DateTime::parse_from_rfc3339(&queued_at)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid queued_at {queued_at}: {e}"),
                    )),
                )
            })?,
        updated_at: DateTime::parse_from_rfc3339(&updated_at)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid updated_at {updated_at}: {e}"),
                    )),
                )
            })?,
        generation: generation.max(0) as u32,
    })
}

fn phase_to_string(phase: &Phase) -> CaduceusResult<String> {
    serde_json::to_string(phase)
        .map(|s| s.trim_matches('"').to_string())
        .map_err(|e| CaduceusError::StateCorrupt {
            path: PathBuf::new(),
            message: format!("cannot serialize phase: {e}"),
        })
}

fn ticket_type_to_string(ticket_type: &TicketType) -> CaduceusResult<String> {
    serde_json::to_string(ticket_type)
        .map(|s| s.trim_matches('"').to_string())
        .map_err(|e| CaduceusError::StateCorrupt {
            path: PathBuf::new(),
            message: format!("cannot serialize ticket_type: {e}"),
        })
}

fn persist_sqlite(conn: &Connection, state: &QueueState) -> CaduceusResult<()> {
    // If we are inside an exclusive section we reuse its transaction,
    // otherwise create a short-lived one for this single write.
    let owns_tx = !is_in_sqlite_tx();
    if owns_tx {
        conn.execute("BEGIN IMMEDIATE", [])
            .map_err(|e| CaduceusError::StateCorrupt {
                path: PathBuf::new(),
                message: format!("cannot begin persist transaction: {e}"),
            })?;
    }
    let result = (|| {
        conn.execute("DELETE FROM queue_entries", [])
            .map_err(|e| CaduceusError::StateCorrupt {
                path: PathBuf::new(),
                message: format!("cannot clear queue_entries: {e}"),
            })?;
        for entry in state.entries.values() {
            let key = entry.key.display_key();
            let phase = phase_to_string(&entry.phase)?;
            let ticket_type = ticket_type_to_string(&entry.ticket_type)?;
            conn.execute(
                "INSERT OR REPLACE INTO queue_entries
                 (issue_key, phase, ticket_type, attempts, last_error, last_run_id,
                  next_attempt_at, finalization, queued_at, updated_at, generation)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    key,
                    phase,
                    ticket_type,
                    entry.attempts as i64,
                    entry.last_error,
                    entry.last_run_id,
                    entry.next_attempt_at.map(|dt| dt.to_rfc3339()),
                    entry
                        .finalization
                        .as_ref()
                        .map(|c| serde_json::to_string(c).unwrap_or_default()),
                    entry.queued_at.to_rfc3339(),
                    entry.updated_at.to_rfc3339(),
                    entry.generation as i64,
                ],
            )
            .map_err(|e| CaduceusError::StateCorrupt {
                path: PathBuf::new(),
                message: format!("cannot persist queue entry {key}: {e}"),
            })?;
        }
        Ok(())
    })();
    if owns_tx {
        if result.is_ok() {
            conn.execute("COMMIT", [])
                .map_err(|e| CaduceusError::StateCorrupt {
                    path: PathBuf::new(),
                    message: format!("cannot commit persist transaction: {e}"),
                })?;
        } else {
            let _ = conn.execute("ROLLBACK", []);
        }
    }
    result
}

fn is_in_sqlite_tx() -> bool {
    SQLITE_TX_CONN.with(|cell| cell.borrow().is_some())
}
