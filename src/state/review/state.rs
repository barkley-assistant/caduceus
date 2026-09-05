//! Per-`(repo, pr)` review state store and the [`ReviewStore`]
//! facade over both backends (issue #295, DAR §4.2): the JSON side is
//! three v1-born versioned files under `<state_dir>/review.lock`;
//! the SQLite side is three tables in the same `state.db` under the
//! v8 envelope.
//!
//! Connection discipline: every load/persist helper takes a
//! [`ReviewConn`] so SQLite operations run on the SAME connection
//! that `with_exclusive` opened and started `BEGIN IMMEDIATE` on —
//! one operation = one connection = one transaction, with no
//! thread-local machinery (review mutations are single-shot and
//! never nest).

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use rusqlite::params;

use super::{
    require_review_file_version, review_queue_key, review_state_key, validate_history_row,
    validated_review_state, ClaimedReview, ReviewClaimFileBody, ReviewClaimToken,
    ReviewEnqueueOutcome, ReviewHistoryFile, ReviewHistoryRow, ReviewPhase, ReviewQueueEntry,
    ReviewQueueState, REVIEW_CLAIMS_DIRNAME, REVIEW_CLAIM_FILE_VERSION, REVIEW_HISTORY_FILENAME,
    REVIEW_HISTORY_FILE_VERSION, REVIEW_LOCK_FILENAME, REVIEW_QUEUE_FILENAME,
    REVIEW_QUEUE_FILE_VERSION, REVIEW_STATE_FILENAME, REVIEW_STATE_FILE_VERSION,
};
use crate::infra::error::{scrub, CaduceusError, CaduceusResult};
use crate::review::{RepositoryId, ReviewState, ReviewTarget};
use crate::state::queue::{atomic_write, process_start_identity, sync_dir};

// ---------------------------------------------------------------------------
// Parse / serialize (pure functions; the test seam)
// ---------------------------------------------------------------------------

/// Parse + validate `review_queue.json` from text. Strict at every
/// layer: unknown fields rejected by serde, envelope version must
/// equal [`REVIEW_QUEUE_FILE_VERSION`] (either direction is
/// unsupported — these files are born at v1 with their first
/// reader), every entry validated, and every map key must equal the
/// canonical [`review_queue_key`] of its entry.
pub fn parse_review_queue_state(text: &str) -> CaduceusResult<ReviewQueueState> {
    const SCOPE: &str = "<review-queue>";
    let state: ReviewQueueState =
        serde_json::from_str(text).map_err(|err| CaduceusError::StateCorrupt {
            path: PathBuf::from(SCOPE),
            message: format!("review queue JSON parse: {err}"),
        })?;
    require_review_file_version(SCOPE, state.version, REVIEW_QUEUE_FILE_VERSION)?;
    for (map_key, entry) in &state.entries {
        let expected = review_queue_key(&entry.target);
        if map_key != &expected {
            return Err(CaduceusError::StateCorrupt {
                path: PathBuf::from(SCOPE),
                message: format!(
                    "review queue map key {map_key:?} does not match entry {expected}"
                ),
            });
        }
        crate::review::validate_review_target(&entry.target).map_err(|e| {
            CaduceusError::StateCorrupt {
                path: PathBuf::from(SCOPE),
                message: format!("review queue entry invalid: {e}"),
            }
        })?;
        if entry.review_generation == 0 {
            return Err(CaduceusError::StateCorrupt {
                path: PathBuf::from(SCOPE),
                message: format!("review queue entry {map_key} has review_generation 0"),
            });
        }
    }
    Ok(state)
}

/// Serialize the review queue state to canonical one-line JSON.
pub fn serialize_review_queue_state(state: &ReviewQueueState) -> CaduceusResult<String> {
    serde_json::to_string(state).map_err(|err| CaduceusError::StateCorrupt {
        path: PathBuf::from("<review-queue>"),
        message: format!("review queue JSON serialize: {err}"),
    })
}

/// Parse + validate `review_state.json` from text (same strictness
/// contract as [`parse_review_queue_state`]; map keys are the
/// lowercase [`review_state_key`] form).
pub fn parse_review_state_map(text: &str) -> CaduceusResult<ReviewStateMap> {
    const SCOPE: &str = "<review-state>";
    let map: ReviewStateMap =
        serde_json::from_str(text).map_err(|err| CaduceusError::StateCorrupt {
            path: PathBuf::from(SCOPE),
            message: format!("review state JSON parse: {err}"),
        })?;
    require_review_file_version(SCOPE, map.version, REVIEW_STATE_FILE_VERSION)?;
    for (map_key, state) in &map.states {
        let expected = review_state_key(&state.repository, state.pull_request);
        if map_key != &expected {
            return Err(CaduceusError::StateCorrupt {
                path: PathBuf::from(SCOPE),
                message: format!(
                    "review state map key {map_key:?} does not match entry {expected}"
                ),
            });
        }
        validated_review_state(state)?;
    }
    Ok(map)
}

/// Serialize the review state map to canonical one-line JSON.
pub fn serialize_review_state_map(map: &ReviewStateMap) -> CaduceusResult<String> {
    serde_json::to_string(map).map_err(|err| CaduceusError::StateCorrupt {
        path: PathBuf::from("<review-state>"),
        message: format!("review state JSON serialize: {err}"),
    })
}

/// Parse + validate `review_history.json` from text: append order is
/// the row order, and every row's identity fields and durable result
/// blob are validated.
pub fn parse_review_history(text: &str) -> CaduceusResult<ReviewHistoryFile> {
    const SCOPE: &str = "<review-history>";
    let file: ReviewHistoryFile =
        serde_json::from_str(text).map_err(|err| CaduceusError::StateCorrupt {
            path: PathBuf::from(SCOPE),
            message: format!("review history JSON parse: {err}"),
        })?;
    require_review_file_version(SCOPE, file.version, REVIEW_HISTORY_FILE_VERSION)?;
    for row in &file.rows {
        validate_history_row(row)?;
    }
    Ok(file)
}

/// Serialize the review history to canonical one-line JSON.
pub fn serialize_review_history(file: &ReviewHistoryFile) -> CaduceusResult<String> {
    serde_json::to_string(file).map_err(|err| CaduceusError::StateCorrupt {
        path: PathBuf::from("<review-history>"),
        message: format!("review history JSON serialize: {err}"),
    })
}

// ---------------------------------------------------------------------------
// Connection handle
// ---------------------------------------------------------------------------

/// Versioned `review_state.json` envelope: one `ReviewState` per
/// `(repo, pr)`, keyed by the lowercase [`review_state_key`] form.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewStateMap {
    pub version: u32,
    pub states: BTreeMap<String, ReviewState>,
}

impl ReviewStateMap {
    pub fn empty() -> Self {
        Self {
            version: REVIEW_STATE_FILE_VERSION,
            states: BTreeMap::new(),
        }
    }
}

/// The connection a load/persist helper must use. `Json` carries no
/// connection; `Sqlite` borrows the one connection the enclosing
/// section opened (inside `with_exclusive` that connection is inside
/// its `BEGIN IMMEDIATE` transaction, so the writes join it).
enum ReviewConn<'a> {
    Json,
    Sqlite(&'a rusqlite::Connection),
}

// ---------------------------------------------------------------------------
// ReviewStore facade
// ---------------------------------------------------------------------------

/// Back-end implementation for [`ReviewStore`].
#[derive(Debug)]
enum ReviewStoreBackend {
    /// Three JSON files per the module docs; lock = `review.lock`.
    Json,
    /// Three tables in `state.db`.
    Sqlite,
}

/// Handle over the three review-era stores (queue, per-PR state,
/// append-only history), mirroring [`crate::state::queue::StateStore`]'s
/// shape. JSON mutations serialise on an exclusive `flock` of
/// `<state_dir>/review.lock` (deliberately NOT `state.lock`, so the
/// two failure domains never nest); SQLite mutations run one
/// `BEGIN IMMEDIATE` per operation.
#[derive(Debug)]
pub struct ReviewStore {
    state_dir: PathBuf,
    claims_dir: PathBuf,
    backend: ReviewStoreBackend,
}

impl ReviewStore {
    /// Open the JSON-backed store. Creates the state directory and
    /// `review-claims/` if missing, then forces a validated load of
    /// all three files so a corrupt file is reported at open time.
    /// A missing file is the empty envelope.
    pub fn open(state_dir: &Path) -> CaduceusResult<Self> {
        fs::create_dir_all(state_dir)?;
        let claims_dir = state_dir.join(REVIEW_CLAIMS_DIRNAME);
        fs::create_dir_all(&claims_dir)?;
        let store = Self {
            state_dir: state_dir.to_path_buf(),
            claims_dir,
            backend: ReviewStoreBackend::Json,
        };
        store.load_queue(&ReviewConn::Json)?;
        store.load_state_map(&ReviewConn::Json)?;
        store.load_history(&ReviewConn::Json)?;
        Ok(store)
    }

    /// Open a SQLite-backed store. Ensures the database schema (the
    /// migration chain runs on first open — a v7 store migrates to
    /// v8 here), then forces a validated load of all three tables.
    /// Claim files remain on disk for both backends.
    pub fn open_sqlite(state_dir: &Path) -> CaduceusResult<Self> {
        fs::create_dir_all(state_dir)?;
        let claims_dir = state_dir.join(REVIEW_CLAIMS_DIRNAME);
        fs::create_dir_all(&claims_dir)?;
        // Open once to run the migration chain / create the schema;
        // operations below open their own short-lived connections so
        // the store stays Send/Sync.
        let conn = crate::state::store::open_in(state_dir)?;
        let store = Self {
            state_dir: state_dir.to_path_buf(),
            claims_dir,
            backend: ReviewStoreBackend::Sqlite,
        };
        store.load_queue(&ReviewConn::Sqlite(&conn))?;
        store.load_state_map(&ReviewConn::Sqlite(&conn))?;
        store.load_history(&ReviewConn::Sqlite(&conn))?;
        Ok(store)
    }

    /// Directory backing this store.
    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.clone()
    }

    /// Review claims directory (`<state_dir>/review-claims`).
    pub fn claims_dir(&self) -> PathBuf {
        self.claims_dir.clone()
    }

    // --- load helpers ------------------------------------------------------

    fn load_queue(&self, conn: &ReviewConn) -> CaduceusResult<ReviewQueueState> {
        match conn {
            ReviewConn::Json => load_queue_json(&self.state_dir.join(REVIEW_QUEUE_FILENAME)),
            ReviewConn::Sqlite(conn) => load_queue_sqlite(conn, &self.state_dir),
        }
    }

    fn load_state_map(&self, conn: &ReviewConn) -> CaduceusResult<ReviewStateMap> {
        match conn {
            ReviewConn::Json => load_state_map_json(&self.state_dir.join(REVIEW_STATE_FILENAME)),
            ReviewConn::Sqlite(conn) => load_state_map_sqlite(conn, &self.state_dir),
        }
    }

    fn load_history(&self, conn: &ReviewConn) -> CaduceusResult<ReviewHistoryFile> {
        match conn {
            ReviewConn::Json => load_history_json(&self.state_dir.join(REVIEW_HISTORY_FILENAME)),
            ReviewConn::Sqlite(conn) => load_history_sqlite(conn, &self.state_dir),
        }
    }

    // --- review queue ------------------------------------------------------

    /// Snapshot the review queue (validated load; never writes).
    pub fn review_queue_snapshot(&self) -> CaduceusResult<ReviewQueueState> {
        self.with_shared(|store, conn| store.load_queue(conn))
    }

    /// Enqueue a new review target. Assigns
    /// `review_generation = current ReviewState generation + 1`
    /// (or 1 when none), upserts the `ReviewState` row's generation,
    /// and inserts the queue entry at phase `Queued`. Returns
    /// [`ReviewEnqueueOutcome::AlreadyPresent`] when an ACTIVE
    /// (`Queued`/`InProgress`) entry already exists for the same
    /// canonical review key.
    ///
    /// The queue entry and the state row always carry the SAME
    /// `review_generation`: both writes happen inside one exclusive
    /// section (one flock / one transaction), so a crash between the
    /// two writes is impossible. This is the load-bearing invariant
    /// for the stale-publication guard (DAR §9.4).
    pub fn enqueue_review(&self, target: &ReviewTarget) -> CaduceusResult<ReviewEnqueueOutcome> {
        crate::review::validate_review_target(target)?;
        self.with_exclusive(|store, conn| {
            let mut queue = store.load_queue(conn)?;
            let mut states = store.load_state_map(conn)?;
            let now = Utc::now();
            let key = review_queue_key(target);

            if let Some(existing) = queue.entries.get(&key) {
                if existing.phase.is_active() {
                    return Ok(ReviewEnqueueOutcome::AlreadyPresent);
                }
            }

            let state_key = review_state_key(&target.repository, target.pull_request);
            let generation = match states.states.get_mut(&state_key) {
                Some(existing_state) => {
                    let generation = existing_state.review_generation + 1;
                    existing_state.review_generation = generation;
                    generation
                }
                None => {
                    let fresh = ReviewState::new(target.repository.clone(), target.pull_request, 1);
                    states.states.insert(state_key, fresh);
                    1
                }
            };

            queue.entries.insert(
                key,
                ReviewQueueEntry {
                    target: target.clone(),
                    phase: ReviewPhase::Queued,
                    attempts: 0,
                    last_error: None,
                    last_run_id: None,
                    next_attempt_at: None,
                    queued_at: now,
                    updated_at: now,
                    review_generation: generation,
                },
            );
            store.persist_queue(conn, &queue)?;
            store.persist_state_map(conn, &states)?;
            Ok(ReviewEnqueueOutcome::Inserted)
        })
    }

    /// Atomically claim the oldest eligible `Queued` entry (its
    /// `next_attempt_at` elapsed or `None`). The claim file is
    /// created first with `O_CREAT | O_EXCL` under
    /// `review-claims/`, then the entry is marked `InProgress` with
    /// `last_run_id` set. If the queue rewrite fails the claim file
    /// is removed so the entry can be re-claimed. Returns `None`
    /// when no eligible entry exists.
    pub fn acquire_next_review(
        &self,
        run_id: &str,
        pid: u32,
        now: DateTime<Utc>,
    ) -> CaduceusResult<Option<ClaimedReview>> {
        if run_id.is_empty() || run_id.len() > 64 {
            return Err(CaduceusError::Queue {
                context: "review-claim",
                stderr: format!(
                    "invalid run_id: must be non-empty and at most 64 bytes (got {})",
                    run_id.len()
                ),
            });
        }
        self.with_exclusive(|store, conn| {
            let mut queue = store.load_queue(conn)?;
            let mut eligible: Vec<(String, ReviewQueueEntry)> = queue
                .entries
                .iter()
                .filter(|(_, e)| e.phase == ReviewPhase::Queued)
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

            for (key, mut entry) in eligible {
                let digest = super::review_claim_digest(&key);
                let claim_path = store.claims_dir.join(format!("{digest}.claim"));
                let claim_file = match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&claim_path)
                {
                    Ok(f) => f,
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                        // Race-loss: another process claimed this
                        // entry; try the next FIFO candidate.
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                };
                let body = ReviewClaimFileBody {
                    version: REVIEW_CLAIM_FILE_VERSION,
                    target: entry.target.clone(),
                    run_id: run_id.to_string(),
                    pid,
                    process_start_identity: process_start_identity(&store.state_dir, pid),
                    started_at: now,
                    worktree_path: None,
                };
                let body_text = match serde_json::to_string(&body) {
                    Ok(text) => text,
                    Err(err) => {
                        let _ = fs::remove_file(&claim_path);
                        return Err(CaduceusError::Queue {
                            context: "review-claim",
                            stderr: format!("serialize claim: {err}"),
                        });
                    }
                };
                if let Err(err) = write_and_sync_claim(&claim_file, body_text.as_bytes()) {
                    let _ = fs::remove_file(&claim_path);
                    return Err(err);
                }
                if let Err(err) = sync_dir(&store.claims_dir) {
                    tracing::debug!(error = %err, "review claim dir sync");
                }

                entry.phase = ReviewPhase::InProgress;
                entry.last_run_id = Some(run_id.to_string());
                entry.updated_at = now;
                queue.entries.insert(key.clone(), entry.clone());
                if let Err(err) = store.persist_queue(conn, &queue) {
                    // Roll back the claim file so the entry can be
                    // re-claimed on the next tick.
                    if let Err(rm_err) = fs::remove_file(&claim_path) {
                        tracing::warn!(
                            error = %rm_err,
                            path = %claim_path.display(),
                            "review claim rollback after persist failure failed"
                        );
                    }
                    return Err(err);
                }
                return Ok(Some(ClaimedReview {
                    entry,
                    claim: ReviewClaimToken::new(
                        store.claims_dir.clone(),
                        digest,
                        run_id.to_string(),
                    ),
                }));
            }
            Ok(None)
        })
    }

    /// Terminal transition for a successful review run: the entry
    /// moves to `Done`.
    pub fn complete_review(&self, claim: ReviewClaimToken) -> CaduceusResult<()> {
        self.terminal_review(claim, ReviewPhase::Done, None)
    }

    /// Retry-or-fail terminal transition, mirroring the issue
    /// queue's `retry_or_fail`: `attempts += 1`; `attempts >= budget`
    /// moves to `Failed`, otherwise back to `Queued` with
    /// `next_attempt_at = now + 300s`. Returns the new phase.
    pub fn retry_or_fail_review(
        &self,
        claim: ReviewClaimToken,
        error: &str,
        budget: u32,
    ) -> CaduceusResult<ReviewPhase> {
        if budget == 0 {
            return Err(CaduceusError::Config(
                "retry_or_fail_review budget must be > 0".to_string(),
            ));
        }
        self.with_exclusive(|store, conn| {
            let mut queue = store.load_queue(conn)?;
            let entry = review_entry_for_claim(&mut queue, &claim)?;
            entry.attempts = entry.attempts.saturating_add(1);
            entry.last_error = Some(error.to_string());
            entry.last_run_id = None;
            let phase = if entry.attempts >= budget {
                entry.phase = ReviewPhase::Failed;
                entry.next_attempt_at = None;
                entry.phase
            } else {
                entry.phase = ReviewPhase::Queued;
                entry.next_attempt_at = Some(Utc::now() + chrono::Duration::seconds(300));
                entry.phase
            };
            entry.updated_at = Utc::now();
            store.persist_queue(conn, &queue)?;
            unlink_review_claim_best_effort(&store.claims_dir, &claim);
            Ok(phase)
        })
    }

    /// Operator-driven skip: the entry moves to `Skipped` and the
    /// reason is recorded on it (overwriting any prior `last_error`).
    pub fn skip_review(&self, claim: ReviewClaimToken, reason: &str) -> CaduceusResult<()> {
        self.terminal_review(claim, ReviewPhase::Skipped, Some(reason))
    }

    fn terminal_review(
        &self,
        claim: ReviewClaimToken,
        phase: ReviewPhase,
        reason: Option<&str>,
    ) -> CaduceusResult<()> {
        self.with_exclusive(|store, conn| {
            let mut queue = store.load_queue(conn)?;
            let entry = review_entry_for_claim(&mut queue, &claim)?;
            entry.phase = phase;
            entry.last_error = reason.map(|r| r.to_string());
            entry.last_run_id = None;
            entry.next_attempt_at = None;
            entry.updated_at = Utc::now();
            store.persist_queue(conn, &queue)?;
            unlink_review_claim_best_effort(&store.claims_dir, &claim);
            Ok(())
        })
    }

    // --- review state ------------------------------------------------------

    /// Upsert the per-`(repo, pr)` state row. CAS guard (DAR §9.4):
    /// rejects the write when `state.review_generation` is LOWER than
    /// the stored generation — an older generation may append history
    /// but never regress the current pointer.
    pub fn save_review_state(&self, state: &ReviewState) -> CaduceusResult<()> {
        validated_review_state(state)?;
        self.with_exclusive(|store, conn| {
            let mut states = store.load_state_map(conn)?;
            let key = review_state_key(&state.repository, state.pull_request);
            if let Some(existing) = states.states.get(&key) {
                if state.review_generation < existing.review_generation {
                    return Err(CaduceusError::StateCorrupt {
                        path: PathBuf::from("<review-state>"),
                        message: format!(
                            "review_generation regression for {key}: stored {}, incoming {}",
                            existing.review_generation, state.review_generation
                        ),
                    });
                }
            }
            states.states.insert(key, state.clone());
            store.persist_state_map(conn, &states)
        })
    }

    /// Read the per-`(repo, pr)` state row, if any.
    pub fn review_state(
        &self,
        repo: &RepositoryId,
        pr: u64,
    ) -> CaduceusResult<Option<ReviewState>> {
        self.with_shared(|store, conn| {
            let states = store.load_state_map(conn)?;
            Ok(states.states.get(&review_state_key(repo, pr)).cloned())
        })
    }

    // --- same-SHA dedup (AC5) ----------------------------------------------

    /// True when the exact `(repo, pr, head_sha)` is already
    /// completed (`ReviewState.last_reviewed_head_sha == Some(sha)`)
    /// OR active (`Queued`/`InProgress`) in the review queue. A
    /// read path over the existing stores — NOT a uniqueness
    /// constraint anywhere; history is never consulted.
    pub fn is_active_or_reviewed(&self, target: &ReviewTarget) -> CaduceusResult<bool> {
        crate::review::validate_review_target(target)?;
        self.with_shared(|store, conn| {
            let key = review_queue_key(target);
            let queue = store.load_queue(conn)?;
            if let Some(entry) = queue.entries.get(&key) {
                if entry.phase.is_active() {
                    return Ok(true);
                }
            }
            let states = store.load_state_map(conn)?;
            let state = states
                .states
                .get(&review_state_key(&target.repository, target.pull_request));
            Ok(state
                .map(|s| {
                    s.last_reviewed_head_sha
                        .as_deref()
                        .map(|sha| sha == target.head_sha)
                        .unwrap_or(false)
                })
                .unwrap_or(false))
        })
    }

    // --- review history ----------------------------------------------------

    /// Append one completed run. Rejects a duplicate
    /// `review_run_id`: an idempotent re-append of the SAME run is an
    /// error, not a dedup — the caller retried a completed
    /// finalization and must not double-write (DAR §9.1 resume is
    /// idempotent via `sticky_comment_id`, not via history re-append).
    pub fn append_history(&self, row: ReviewHistoryRow) -> CaduceusResult<()> {
        validate_history_row(&row)?;
        self.with_exclusive(|store, conn| {
            let mut history = store.load_history(conn)?;
            if history
                .rows
                .iter()
                .any(|r| r.review_run_id == row.review_run_id)
            {
                return Err(CaduceusError::StateCorrupt {
                    path: PathBuf::from("<review-history>"),
                    message: format!(
                        "duplicate review_run_id {} — completed runs are never re-appended",
                        row.review_run_id
                    ),
                });
            }
            history.rows.push(row);
            store.persist_history(conn, &history)
        })
    }

    /// Rows for one `(repo, pr)`, oldest first (append order).
    pub fn history_for_pull_request(
        &self,
        repo: &RepositoryId,
        pr: u64,
    ) -> CaduceusResult<Vec<ReviewHistoryRow>> {
        let owner = repo.owner.to_lowercase();
        let name = repo.repo.to_lowercase();
        self.with_shared(|store, conn| {
            let history = store.load_history(conn)?;
            Ok(history
                .rows
                .into_iter()
                .filter(|r| {
                    r.repository.owner.to_lowercase() == owner
                        && r.repository.repo.to_lowercase() == name
                        && r.pull_request == pr
                })
                .collect())
        })
    }

    /// Rows for one `(repo, pr, head_sha)` — MAY be more than one
    /// (append-only identity is the run id, not the tuple).
    pub fn history_for_head_sha(
        &self,
        repo: &RepositoryId,
        pr: u64,
        head_sha: &str,
    ) -> CaduceusResult<Vec<ReviewHistoryRow>> {
        Ok(self
            .history_for_pull_request(repo, pr)?
            .into_iter()
            .filter(|r| r.head_sha == head_sha)
            .collect())
    }

    /// All rows for one repository (append order).
    pub fn history_for_repository(
        &self,
        repo: &RepositoryId,
    ) -> CaduceusResult<Vec<ReviewHistoryRow>> {
        let owner = repo.owner.to_lowercase();
        let name = repo.repo.to_lowercase();
        self.with_shared(|store, conn| {
            let history = store.load_history(conn)?;
            Ok(history
                .rows
                .into_iter()
                .filter(|r| {
                    r.repository.owner.to_lowercase() == owner
                        && r.repository.repo.to_lowercase() == name
                })
                .collect())
        })
    }

    // --- lock helpers -------------------------------------------------------

    /// Shared-section read. JSON takes a shared `review.lock` flock;
    /// SQLite opens a fresh connection (WAL allows concurrent
    /// readers).
    fn with_shared<R>(
        &self,
        op: impl FnOnce(&Self, &ReviewConn) -> CaduceusResult<R>,
    ) -> CaduceusResult<R> {
        match &self.backend {
            ReviewStoreBackend::Json => {
                let lock_path = self.state_dir.join(REVIEW_LOCK_FILENAME);
                let lock_file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&lock_path)?;
                lock_file.lock_shared().map_err(into_lock_error)?;
                let result = op(self, &ReviewConn::Json);
                if let Err(err) = lock_file.unlock() {
                    tracing::debug!(
                        error = %scrub(&format!("{err:?}")),
                        "review shared unlock"
                    );
                }
                result
            }
            ReviewStoreBackend::Sqlite => {
                let conn = crate::state::store::open_in(&self.state_dir)?;
                op(self, &ReviewConn::Sqlite(&conn))
            }
        }
    }

    /// Exclusive section: JSON takes the `review.lock` flock; SQLite
    /// opens one connection and wraps the operation in a single
    /// `BEGIN IMMEDIATE` transaction. The connection handle is passed
    /// to the operation so every load/persist inside it joins the
    /// same transaction.
    fn with_exclusive<R>(
        &self,
        op: impl FnOnce(&Self, &ReviewConn) -> CaduceusResult<R>,
    ) -> CaduceusResult<R> {
        match &self.backend {
            ReviewStoreBackend::Json => {
                let lock_path = self.state_dir.join(REVIEW_LOCK_FILENAME);
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&lock_path)?;
                file.lock_exclusive().map_err(into_lock_error)?;
                let result = op(self, &ReviewConn::Json);
                if let Err(err) = file.unlock() {
                    tracing::debug!(
                        error = %scrub(&format!("{err:?}")),
                        "review exclusive unlock"
                    );
                }
                result
            }
            ReviewStoreBackend::Sqlite => {
                let conn = crate::state::store::open_in(&self.state_dir)?;
                conn.execute("BEGIN IMMEDIATE", [])
                    .map_err(|e| CaduceusError::StateCorrupt {
                        path: self.state_dir.join(crate::state::store::DB_FILENAME),
                        message: format!("cannot begin review IMMEDIATE transaction: {e}"),
                    })?;
                let result = op(self, &ReviewConn::Sqlite(&conn));
                match &result {
                    Ok(_) => {
                        if let Err(e) = conn.execute("COMMIT", []) {
                            return Err(CaduceusError::StateCorrupt {
                                path: self.state_dir.join(crate::state::store::DB_FILENAME),
                                message: format!("cannot commit review transaction: {e}"),
                            });
                        }
                        result
                    }
                    Err(_) => {
                        let _ = conn.execute("ROLLBACK", []);
                        result
                    }
                }
            }
        }
    }

    // --- persist helpers ----------------------------------------------------

    fn persist_queue(&self, conn: &ReviewConn, queue: &ReviewQueueState) -> CaduceusResult<()> {
        match conn {
            ReviewConn::Json => {
                let path = self.state_dir.join(REVIEW_QUEUE_FILENAME);
                let body = serialize_review_queue_state(queue)?;
                atomic_write(&path, body.as_bytes())?;
                sync_dir(&self.state_dir)
            }
            ReviewConn::Sqlite(conn) => persist_queue_sqlite(conn, queue),
        }
    }

    fn persist_state_map(&self, conn: &ReviewConn, states: &ReviewStateMap) -> CaduceusResult<()> {
        match conn {
            ReviewConn::Json => {
                let path = self.state_dir.join(REVIEW_STATE_FILENAME);
                let body = serialize_review_state_map(states)?;
                atomic_write(&path, body.as_bytes())?;
                sync_dir(&self.state_dir)
            }
            ReviewConn::Sqlite(conn) => persist_state_map_sqlite(conn, states),
        }
    }

    fn persist_history(
        &self,
        conn: &ReviewConn,
        history: &ReviewHistoryFile,
    ) -> CaduceusResult<()> {
        match conn {
            ReviewConn::Json => {
                let path = self.state_dir.join(REVIEW_HISTORY_FILENAME);
                let body = serialize_review_history(history)?;
                atomic_write(&path, body.as_bytes())?;
                sync_dir(&self.state_dir)
            }
            ReviewConn::Sqlite(conn) => persist_history_sqlite(conn, history),
        }
    }
}

// ---------------------------------------------------------------------------
// Claim helpers
// ---------------------------------------------------------------------------

fn write_and_sync_claim(file: &std::fs::File, body: &[u8]) -> CaduceusResult<()> {
    use std::io::Write;
    let mut writer = file;
    writer.write_all(body)?;
    writer.sync_all()?;
    set_mode_0600(file)?;
    Ok(())
}

#[cfg(unix)]
fn set_mode_0600(file: &std::fs::File) -> CaduceusResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = file.metadata()?.permissions();
    perms.set_mode(0o600);
    file.set_permissions(perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode_0600(_file: &std::fs::File) -> CaduceusResult<()> {
    Ok(())
}

/// The entry an in-progress claim refers to: phase must be
/// `InProgress`, the run ids must match, and the recomputed digest
/// must match the token (forged-token defence, mirroring
/// `matches_token`).
fn review_entry_for_claim<'a>(
    queue: &'a mut ReviewQueueState,
    claim: &ReviewClaimToken,
) -> CaduceusResult<&'a mut ReviewQueueEntry> {
    queue
        .entries
        .iter_mut()
        .find(|(key, entry)| {
            entry.phase == ReviewPhase::InProgress
                && entry.last_run_id.as_deref() == Some(claim.run_id())
                && super::review_claim_digest(key.as_str()) == *claim.digest()
        })
        .map(|(_, entry)| entry)
        .ok_or_else(|| review_claim_mismatch(claim))
}

fn review_claim_mismatch(claim: &ReviewClaimToken) -> CaduceusError {
    CaduceusError::Queue {
        context: "review-claim-terminal-mismatch",
        stderr: format!(
            "review claim token run_id {:?} digest {} does not match any in-progress review entry",
            claim.run_id(),
            claim.digest()
        ),
    }
}

fn unlink_review_claim_best_effort(claims_dir: &Path, claim: &ReviewClaimToken) {
    let path = claim.claim_path();
    match fs::remove_file(&path) {
        Ok(()) => {
            if let Err(err) = sync_dir(claims_dir) {
                tracing::debug!(error = %err, "review claim-dir sync");
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(
                error = %err,
                path = %path.display(),
                "review claim unlink failed"
            );
        }
    }
}

fn into_lock_error(err: std::io::Error) -> CaduceusError {
    CaduceusError::Io(err)
}

// ---------------------------------------------------------------------------
// JSON load helpers
// ---------------------------------------------------------------------------

fn read_utf8(path: &Path, what: &str) -> CaduceusResult<String> {
    let bytes = fs::read(path).map_err(|err| CaduceusError::StateCorrupt {
        path: path.to_path_buf(),
        message: format!("cannot read {what}: {err}"),
    })?;
    String::from_utf8(bytes).map_err(|err| CaduceusError::StateCorrupt {
        path: path.to_path_buf(),
        message: format!("{what} is not UTF-8: {err}"),
    })
}

fn load_queue_json(path: &Path) -> CaduceusResult<ReviewQueueState> {
    match fs::read(path) {
        Ok(_) => parse_review_queue_state(&read_utf8(path, "review queue file")?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ReviewQueueState::empty()),
        Err(err) => Err(err.into()),
    }
}

fn load_state_map_json(path: &Path) -> CaduceusResult<ReviewStateMap> {
    match fs::read(path) {
        Ok(_) => parse_review_state_map(&read_utf8(path, "review state file")?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ReviewStateMap::empty()),
        Err(err) => Err(err.into()),
    }
}

fn load_history_json(path: &Path) -> CaduceusResult<ReviewHistoryFile> {
    match fs::read(path) {
        Ok(_) => parse_review_history(&read_utf8(path, "review history file")?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ReviewHistoryFile::empty()),
        Err(err) => Err(err.into()),
    }
}

// ---------------------------------------------------------------------------
// SQLite row mapping
// ---------------------------------------------------------------------------

fn corrupt(path: &Path, message: String) -> CaduceusError {
    CaduceusError::StateCorrupt {
        path: path.to_path_buf(),
        message,
    }
}

fn corrupt_store(message: String) -> CaduceusError {
    CaduceusError::StateCorrupt {
        path: PathBuf::from("<review-store>"),
        message,
    }
}

fn parse_pr_u64(value: i64) -> CaduceusResult<u64> {
    u64::try_from(value).map_err(|_| corrupt_store(format!("negative pull_request value {value}")))
}

fn parse_generation(value: i64) -> u64 {
    value.max(0) as u64
}

fn repository_from_columns(owner: &str, repo: &str) -> RepositoryId {
    RepositoryId {
        owner: owner.to_string(),
        repo: repo.to_string(),
    }
}

fn parse_timestamp(value: String, field: &str, db_path: &Path) -> CaduceusResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| corrupt(db_path, format!("invalid {field} timestamp {value:?}: {e}")))
}

fn parse_optional_timestamp(
    value: Option<String>,
    field: &str,
    db_path: &Path,
) -> CaduceusResult<Option<DateTime<Utc>>> {
    match value {
        None => Ok(None),
        Some(v) => Ok(Some(parse_timestamp(v, field, db_path)?)),
    }
}

fn enum_label<T: serde::de::DeserializeOwned>(
    label: &str,
    field: &str,
    db_path: &Path,
) -> CaduceusResult<T> {
    serde_json::from_str::<T>(&format!("\"{label}\""))
        .map_err(|_| corrupt(db_path, format!("invalid {field} label {label:?}")))
}

fn load_queue_sqlite(
    conn: &rusqlite::Connection,
    state_dir: &Path,
) -> CaduceusResult<ReviewQueueState> {
    let db_path = state_dir.join(crate::state::store::DB_FILENAME);
    let mut stmt = conn
        .prepare(
            "SELECT review_key, owner, repo, pull_request, head_sha, base_sha, base_ref,
                    merge_base, phase, attempts, last_error, last_run_id, next_attempt_at,
                    queued_at, updated_at, review_generation
             FROM review_queue_entries",
        )
        .map_err(|e| {
            corrupt(
                &db_path,
                format!("cannot prepare review_queue_entries select: {e}"),
            )
        })?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, String>(14)?,
                row.get::<_, i64>(15)?,
            ))
        })
        .map_err(|e| corrupt(&db_path, format!("cannot read review_queue_entries: {e}")))?;

    let mut entries = BTreeMap::new();
    for row in rows {
        let (
            review_key,
            owner,
            repo,
            pull_request,
            head_sha,
            base_sha,
            base_ref,
            merge_base,
            phase_str,
            attempts,
            last_error,
            last_run_id,
            next_attempt_at,
            queued_at,
            updated_at,
            review_generation,
        ) = row.map_err(|e| corrupt(&db_path, format!("cannot decode review queue row: {e}")))?;
        let pull_request =
            parse_pr_u64(pull_request).map_err(|e| corrupt(&db_path, e.to_string()))?;
        let target = ReviewTarget {
            repository: repository_from_columns(&owner, &repo),
            pull_request,
            head_sha,
            base_sha,
            base_ref,
            merge_base,
        };
        let phase = enum_label::<ReviewPhase>(&phase_str, "review phase", &db_path)?;
        let entry = ReviewQueueEntry {
            target,
            phase,
            attempts: attempts.max(0) as u32,
            last_error,
            last_run_id,
            next_attempt_at: parse_optional_timestamp(
                next_attempt_at,
                "next_attempt_at",
                &db_path,
            )?,
            queued_at: parse_timestamp(queued_at, "queued_at", &db_path)?,
            updated_at: parse_timestamp(updated_at, "updated_at", &db_path)?,
            review_generation: parse_generation(review_generation),
        };
        crate::review::validate_review_target(&entry.target)
            .map_err(|e| corrupt(&db_path, format!("review queue row invalid: {e}")))?;
        let key = review_queue_key(&entry.target);
        if review_key != key {
            return Err(corrupt(
                &db_path,
                format!("review queue key drift: stored {review_key:?}, computed {key:?}"),
            ));
        }
        entries.insert(key, entry);
    }
    Ok(ReviewQueueState {
        version: REVIEW_QUEUE_FILE_VERSION,
        entries,
    })
}

fn persist_queue_sqlite(
    conn: &rusqlite::Connection,
    queue: &ReviewQueueState,
) -> CaduceusResult<()> {
    // Mirror the issue queue's DELETE-then-INSERT persist shape; the
    // review queue is tens of rows at Phase-1 scale.
    conn.execute("DELETE FROM review_queue_entries", [])
        .map_err(|e| corrupt_store(format!("cannot clear review_queue_entries: {e}")))?;
    for (key, entry) in &queue.entries {
        conn.execute(
            "INSERT INTO review_queue_entries
             (review_key, owner, repo, pull_request, head_sha, base_sha, base_ref,
              merge_base, phase, attempts, last_error, last_run_id, next_attempt_at,
              queued_at, updated_at, review_generation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                key,
                entry.target.repository.owner.to_lowercase(),
                entry.target.repository.repo.to_lowercase(),
                entry.target.pull_request as i64,
                entry.target.head_sha,
                entry.target.base_sha,
                entry.target.base_ref,
                entry.target.merge_base,
                entry.phase.as_str(),
                entry.attempts as i64,
                entry.last_error,
                entry.last_run_id,
                entry.next_attempt_at.map(|dt| dt.to_rfc3339()),
                entry.queued_at.to_rfc3339(),
                entry.updated_at.to_rfc3339(),
                entry.review_generation as i64,
            ],
        )
        .map_err(|e| corrupt_store(format!("cannot persist review queue entry {key}: {e}")))?;
    }
    Ok(())
}

fn load_state_map_sqlite(
    conn: &rusqlite::Connection,
    state_dir: &Path,
) -> CaduceusResult<ReviewStateMap> {
    let db_path = state_dir.join(crate::state::store::DB_FILENAME);
    let mut stmt = conn
        .prepare(
            "SELECT owner, repo, pull_request, last_reviewed_head_sha, last_verdict,
                    last_reviewed_at, sticky_comment_id, last_run_id, review_generation,
                    publication_state, publication_attempt_count, next_publish_at,
                    last_publish_error
             FROM review_state",
        )
        .map_err(|e| corrupt(&db_path, format!("cannot prepare review_state select: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
            ))
        })
        .map_err(|e| corrupt(&db_path, format!("cannot read review_state: {e}")))?;

    let mut states = BTreeMap::new();
    for row in rows {
        let (
            owner,
            repo,
            pull_request,
            last_reviewed_head_sha,
            last_verdict,
            last_reviewed_at,
            sticky_comment_id,
            last_run_id,
            review_generation,
            publication_state,
            publication_attempt_count,
            next_publish_at,
            last_publish_error,
        ) = row.map_err(|e| corrupt(&db_path, format!("cannot decode review state row: {e}")))?;
        let pr = parse_pr_u64(pull_request).map_err(|e| corrupt(&db_path, e.to_string()))?;
        let last_verdict = match last_verdict.as_deref() {
            None => None,
            Some(label) => Some(enum_label::<crate::review::Verdict>(
                label,
                "last_verdict",
                &db_path,
            )?),
        };
        let publication_state = enum_label::<crate::review::PublicationState>(
            &publication_state,
            "publication_state",
            &db_path,
        )?;
        let sticky_comment_id = match sticky_comment_id {
            Some(v) => Some(
                u64::try_from(v)
                    .map_err(|_| corrupt(&db_path, format!("negative sticky_comment_id {v}")))?,
            ),
            None => None,
        };
        let state = ReviewState {
            repository: repository_from_columns(&owner, &repo),
            pull_request: pr,
            last_reviewed_head_sha,
            last_verdict,
            last_reviewed_at: parse_optional_timestamp(
                last_reviewed_at,
                "last_reviewed_at",
                &db_path,
            )?,
            sticky_comment_id,
            last_run_id,
            review_generation: parse_generation(review_generation),
            publication_state,
            publication_attempt_count: publication_attempt_count.max(0) as u32,
            next_publish_at: parse_optional_timestamp(
                next_publish_at,
                "next_publish_at",
                &db_path,
            )?,
            last_publish_error,
        };
        validated_review_state(&state).map_err(|e| corrupt(&db_path, e.to_string()))?;
        let key = review_state_key(&state.repository, state.pull_request);
        if key != format!("{owner}/{repo}#{pr}") {
            return Err(corrupt(
                &db_path,
                format!("review state key drift: stored {owner}/{repo}#{pr}, computed {key}"),
            ));
        }
        states.insert(key, state);
    }
    Ok(ReviewStateMap {
        version: REVIEW_STATE_FILE_VERSION,
        states,
    })
}

fn persist_state_map_sqlite(
    conn: &rusqlite::Connection,
    states: &ReviewStateMap,
) -> CaduceusResult<()> {
    // Upsert-by-key: one row per (repo, pr), targeted writes (the
    // state store is NOT DELETE-all — the current pointer is
    // per-key).
    for (key, state) in &states.states {
        conn.execute(
            "INSERT INTO review_state
             (owner, repo, pull_request, last_reviewed_head_sha, last_verdict,
              last_reviewed_at, sticky_comment_id, last_run_id, review_generation,
              publication_state, publication_attempt_count, next_publish_at,
              last_publish_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(owner, repo, pull_request) DO UPDATE SET
                last_reviewed_head_sha = excluded.last_reviewed_head_sha,
                last_verdict = excluded.last_verdict,
                last_reviewed_at = excluded.last_reviewed_at,
                sticky_comment_id = excluded.sticky_comment_id,
                last_run_id = excluded.last_run_id,
                review_generation = excluded.review_generation,
                publication_state = excluded.publication_state,
                publication_attempt_count = excluded.publication_attempt_count,
                next_publish_at = excluded.next_publish_at,
                last_publish_error = excluded.last_publish_error",
            params![
                state.repository.owner.to_lowercase(),
                state.repository.repo.to_lowercase(),
                state.pull_request as i64,
                state.last_reviewed_head_sha,
                state.last_verdict.as_ref().map(|v| serde_json::to_string(v)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string()),
                state.last_reviewed_at.map(|dt| dt.to_rfc3339()),
                state.sticky_comment_id.map(|v| v as i64),
                state.last_run_id,
                state.review_generation as i64,
                serde_json::to_string(&state.publication_state)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string(),
                state.publication_attempt_count as i64,
                state.next_publish_at.map(|dt| dt.to_rfc3339()),
                state.last_publish_error,
            ],
        )
        .map_err(|e| corrupt_store(format!("cannot persist review state {key}: {e}")))?;
    }
    Ok(())
}

fn load_history_sqlite(
    conn: &rusqlite::Connection,
    state_dir: &Path,
) -> CaduceusResult<ReviewHistoryFile> {
    let db_path = state_dir.join(crate::state::store::DB_FILENAME);
    let mut stmt = conn
        .prepare(
            "SELECT review_run_id, owner, repo, pull_request, head_sha, review_generation,
                    completed_at, result_json
             FROM review_history
             ORDER BY rowid",
        )
        .map_err(|e| {
            corrupt(
                &db_path,
                format!("cannot prepare review_history select: {e}"),
            )
        })?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(|e| corrupt(&db_path, format!("cannot read review_history: {e}")))?;

    let mut out = Vec::new();
    for row in rows {
        let (
            review_run_id,
            owner,
            repo,
            pull_request,
            head_sha,
            review_generation,
            completed_at,
            result_json,
        ) = row.map_err(|e| corrupt(&db_path, format!("cannot decode review history row: {e}")))?;
        let pr = parse_pr_u64(pull_request).map_err(|e| corrupt(&db_path, e.to_string()))?;
        let row = ReviewHistoryRow {
            review_run_id,
            repository: repository_from_columns(&owner, &repo),
            pull_request: pr,
            head_sha,
            review_generation: parse_generation(review_generation),
            completed_at: parse_timestamp(completed_at, "completed_at", &db_path)?,
            result_json,
        };
        validate_history_row(&row).map_err(|e| corrupt(&db_path, e.to_string()))?;
        out.push(row);
    }
    Ok(ReviewHistoryFile {
        version: REVIEW_HISTORY_FILE_VERSION,
        rows: out,
    })
}

fn persist_history_sqlite(
    conn: &rusqlite::Connection,
    history: &ReviewHistoryFile,
) -> CaduceusResult<()> {
    // INSERT-only: history is append-only (AC3) — no UPDATE/DELETE
    // method exists on this store. The unique run_id PK makes a
    // re-append idempotent no-op at the SQL level while the store's
    // duplicate check makes the second append a hard error above.
    for row in &history.rows {
        conn.execute(
            "INSERT INTO review_history
             (review_run_id, owner, repo, pull_request, head_sha, review_generation,
              completed_at, result_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(review_run_id) DO NOTHING",
            params![
                row.review_run_id,
                row.repository.owner.to_lowercase(),
                row.repository.repo.to_lowercase(),
                row.pull_request as i64,
                row.head_sha,
                row.review_generation as i64,
                row.completed_at.to_rfc3339(),
                row.result_json,
            ],
        )
        .map_err(|e| {
            corrupt_store(format!(
                "cannot append review history row {}: {e}",
                row.review_run_id
            ))
        })?;
    }
    Ok(())
}
