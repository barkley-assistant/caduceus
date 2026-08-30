//! `<state_dir>/state_meta.json` schema, `MetaStore`, and the
//! crash-safe writer. This module owns the canonical metadata
//! envelope and the `RateLimitObserver` defined here so the
//! HTTP layer and the persistence layer can evolve
//! independently.
//!
//! ## Concurrency model
//!
//! Every read-modify-write cycle goes through [`MetaStore::update`],
//! which serialises on an internal `Mutex<StateMeta>`. The write
//! itself uses the same atomic rename strategy as the queue state
//! file: write a temporary file, fsync, rename.
//!
//! ## Corrupt-file handling
//!
//! If [`load`] finds a file that cannot be parsed, the original
//! file is copied to `<state_dir>/state_meta.json.corrupt-<ts>`,
//! a `<state_dir>/state_meta.corrupt` marker is written, and the
//! function returns the original error wrapped as
//! [`CaduceusError::StateCorrupt`]. The active file is *not*
//! deleted. Subsequent ticks refuse to call GitHub while the
//! marker exists; the documented recovery command clears it.
//!
//! ## Diagnostic coalescing
//!
//! `DaemonDiagnostic` entries with the same `(code, issue_key)`
//! within a one-hour window are coalesced rather than appended.
//! The cap is 20 entries (newest 20).
//!
//! ## Rate-limit observer
//!
//! [`RateLimitObserver::observe`] merges a new observation into
//! the persisted metadata without ever overwriting a newer
//! observation with an older one. The check is by
//! `reset_at` timestamp.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::github::issue::IssueKey;
use crate::github::RateLimitInfo;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Current metadata envelope version. Bumping this is a breaking
/// change — older files produce a `StateCorrupt` error.
pub const META_VERSION: u32 = 1;

/// Maximum number of `recent_diagnostics` entries retained.
pub const MAX_DIAGNOSTICS: usize = 20;

/// Window during which duplicate `(code, issue_key)` entries are
/// coalesced rather than appended.
pub const DIAGNOSTIC_COALESCE_WINDOW: Duration = Duration::hours(1);

/// Filename for the active metadata file.
pub const META_FILENAME: &str = "state_meta.json";

/// Marker filename written when corruption is detected.
pub const CORRUPT_MARKER_FILENAME: &str = "state_meta.corrupt";

/// File used by the JSON metadata backend for the daemon's installation
/// identity. It is deliberately separate from the versioned metadata
/// envelope so existing metadata readers remain backward compatible.
pub const INSTALLATION_UUID_FILENAME: &str = "installation_uuid";

/// Persisted tick metadata. Field semantics are pinned by the
/// StateMeta contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateMeta {
    pub version: u32,
    pub last_tick_started: Option<DateTime<Utc>>,
    pub last_tick_finished: Option<DateTime<Utc>>,
    pub last_outcome: Option<TickOutcome>,
    pub last_http_status: Option<u16>,
    pub next_allowed_poll_at: Option<DateTime<Utc>>,
    pub last_reap_at: Option<DateTime<Utc>>,
    pub last_reaped_count: u32,
    pub rate_limit: Option<RateLimitObservation>,
    pub last_error: Option<String>,
    pub recent_diagnostics: Vec<DaemonDiagnostic>,
}

impl StateMeta {
    /// Empty metadata envelope at the current version.
    pub fn empty() -> Self {
        Self {
            version: META_VERSION,
            last_tick_started: None,
            last_tick_finished: None,
            last_outcome: None,
            last_http_status: None,
            next_allowed_poll_at: None,
            last_reap_at: None,
            last_reaped_count: 0,
            rate_limit: None,
            last_error: None,
            recent_diagnostics: Vec::new(),
        }
    }
}

/// Outcome of the most recent tick.
///
/// The variant set is the contractually-documented one.
/// `Idle304` and `IdleEmpty` split the legacy `Idle` variant
/// on whether every poll response reused the persistent HTTP
/// cache. `SkippedConcurrent` and `SkippedCadence` replace
/// the older `Concurrent` and `Cadence` aliases so the
/// orchestrator's perspective is always the actionable one.
/// - The `SkippedCadence` outcome is what the daemon returns
///   when the configured `poll_interval_seconds` has not
///   elapsed since the last tick; the older `Cadence` variant
///   is retained as a low-level meta-layer alias.
/// - The `Idle304` outcome is reserved for the "all polls
///   reused the conditional GET cache" case. Otherwise, an
///   idle poll finishes as `IdleEmpty`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TickOutcome {
    /// A worker run completed successfully (code,
    /// investigation, or dry-run preview).
    Processed,
    /// Every poll response was a cached 304 — no eligible
    /// entry exists.
    Idle304,
    /// At least one poll response was a fresh 200 with no
    /// eligible entry.
    IdleEmpty,
    /// Another `caduceus` invocation holds the daemon.lock.
    SkippedConcurrent,
    /// The configured cadence interval has not elapsed.
    SkippedCadence,
    /// Persisted rate-limit reset has not elapsed.
    RateLimited,
    /// Operator SIGINT/SIGTERM or timeout-driven drain.
    Cancelled,
    /// A configuration / state / invariant / unrecovered
    /// pipeline failure.
    Failed,
}

/// Persisted GitHub rate-limit observation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RateLimitObservation {
    pub limit: Option<u32>,
    pub remaining: u32,
    pub reset_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
}

impl RateLimitObservation {
    /// True if this observation's `reset_at` is strictly newer than
    /// *other*'s. Used by [`RateLimitObserver::observe`] to refuse
    /// older observations from overwriting newer ones.
    pub fn is_newer_than(&self, other: &RateLimitObservation) -> bool {
        self.reset_at > other.reset_at
    }
}

/// One diagnostic entry. Field semantics are pinned by the
/// StateMeta contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonDiagnostic {
    pub timestamp: DateTime<Utc>,
    pub code: String,
    pub issue_key: Option<IssueKey>,
    pub message: String,
}

/// Storage backend for [`MetaStore`]. The JSON variant persists
/// to `state_meta.json` with a corrupt-file marker; the SQLite
/// variant persists to the `state_meta` table in `state.db`.
#[derive(Debug)]
enum MetaStoreBackend {
    Json {
        meta_path: PathBuf,
        corrupt_marker: PathBuf,
    },
    Sqlite(PathBuf),
}

thread_local! {
    /// Thread-local transaction connection used by SQLite-backed
    /// [`MetaStore::update`] so that a sequence of reads and writes
    /// can share one `BEGIN IMMEDIATE` transaction.
    static META_TX_CONN: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

/// Read-modify-write store for `StateMeta`. The mutex serialises
/// concurrent updates so concurrent HTTP responses can merge
/// rate-limit observations without losing fields.
#[derive(Debug)]
pub struct MetaStore {
    state_dir: PathBuf,
    inner: Mutex<StateMeta>,
    backend: MetaStoreBackend,
}

impl MetaStore {
    /// Open an existing metadata file (or initialise an empty
    /// envelope when no file exists yet).
    pub fn open(state_dir: &Path) -> CaduceusResult<Self> {
        let meta_path = state_dir.join(META_FILENAME);
        let corrupt_marker = state_dir.join(CORRUPT_MARKER_FILENAME);
        let meta = match read_active(&meta_path) {
            Ok(m) => m,
            Err(err) => {
                return Err(err);
            }
        };
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            inner: Mutex::new(meta),
            backend: MetaStoreBackend::Json {
                meta_path,
                corrupt_marker,
            },
        })
    }

    /// Open a SQLite-backed metadata store. The database schema is
    /// created if needed and the current `state_meta` rows are read
    /// into the in-memory cache.
    pub fn open_sqlite(state_dir: &Path) -> CaduceusResult<Self> {
        fs::create_dir_all(state_dir)?;
        // Open once to ensure the schema is present.
        let _conn = crate::state::store::open_in(state_dir)?;
        let meta = if let Ok(conn) = crate::state::store::open_in(state_dir) {
            load_sqlite(&conn, state_dir).unwrap_or_else(|_| StateMeta::empty())
        } else {
            StateMeta::empty()
        };
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            inner: Mutex::new(meta),
            backend: MetaStoreBackend::Sqlite(state_dir.to_path_buf()),
        })
    }

    /// Run *f* on a mutable reference to the metadata and persist
    /// the result. The call serialises on the internal mutex so
    /// concurrent updates are safe.
    pub fn update<F>(&self, f: F) -> CaduceusResult<()>
    where
        F: FnOnce(&mut StateMeta),
    {
        let mut guard = self.inner.lock().expect("meta mutex poisoned");
        f(&mut guard);
        match &self.backend {
            MetaStoreBackend::Json { meta_path, .. } => save_atomic(meta_path, &guard),
            MetaStoreBackend::Sqlite(state_dir) => {
                if let Some(conn) = take_meta_tx_conn() {
                    save_sqlite(&conn, state_dir, &guard)
                } else {
                    let conn = crate::state::store::open_in(state_dir)?;
                    conn.execute("BEGIN IMMEDIATE", []).map_err(|e| {
                        CaduceusError::StateCorrupt {
                            path: state_dir.join(crate::state::store::DB_FILENAME),
                            message: format!("cannot begin meta transaction: {e}"),
                        }
                    })?;
                    let res = save_sqlite(&conn, state_dir, &guard);
                    if res.is_ok() {
                        conn.execute("COMMIT", [])
                            .map_err(|e| CaduceusError::StateCorrupt {
                                path: state_dir.join(crate::state::store::DB_FILENAME),
                                message: format!("cannot commit meta transaction: {e}"),
                            })?;
                    } else {
                        let _ = conn.execute("ROLLBACK", []);
                    }
                    res
                }
            }
        }
    }

    /// Borrow the metadata without modifying it.
    pub fn snapshot(&self) -> StateMeta {
        match &self.backend {
            MetaStoreBackend::Json { .. } => {
                self.inner.lock().expect("meta mutex poisoned").clone()
            }
            MetaStoreBackend::Sqlite(state_dir) => {
                // Re-read from SQLite so concurrent writers are
                // visible even when the cache is stale.
                crate::state::store::open_in(state_dir)
                    .and_then(|conn| load_sqlite(&conn, state_dir))
                    .unwrap_or_else(|_| self.inner.lock().expect("meta mutex poisoned").clone())
            }
        }
    }

    /// True when the corrupt marker is present. Always `false` for
    /// the SQLite backend, whose corruption surfaces as open errors
    /// instead of marker files.
    pub fn is_corrupt(&self) -> bool {
        match &self.backend {
            MetaStoreBackend::Json { corrupt_marker, .. } => corrupt_marker.exists(),
            MetaStoreBackend::Sqlite(_) => false,
        }
    }

    /// Clear the corrupt marker after a successful recovery. No-op
    /// for the SQLite backend.
    pub fn clear_corrupt_marker(&self) -> CaduceusResult<()> {
        if let MetaStoreBackend::Json { corrupt_marker, .. } = &self.backend {
            if corrupt_marker.exists() {
                fs::remove_file(corrupt_marker)?;
            }
        }
        Ok(())
    }

    /// Path to the corrupt marker (test seam). For SQLite, returns
    /// the state directory.
    pub fn corrupt_marker_path(&self) -> &Path {
        match &self.backend {
            MetaStoreBackend::Json { corrupt_marker, .. } => corrupt_marker,
            MetaStoreBackend::Sqlite(_) => &self.state_dir,
        }
    }

    /// Path to the active metadata file (test seam). For SQLite,
    /// returns the state directory.
    pub fn meta_path(&self) -> &Path {
        match &self.backend {
            MetaStoreBackend::Json { meta_path, .. } => meta_path,
            MetaStoreBackend::Sqlite(_) => &self.state_dir,
        }
    }

    /// The state directory (test seam).
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Load the installation UUID, generating it once when this is the
    /// first start. JSON uses the shared atomic-write primitive; SQLite
    /// uses one immediate transaction so two writers cannot create two
    /// identities. The persisted value is never overwritten.
    pub fn get_or_create_installation_uuid(&self) -> CaduceusResult<String> {
        let _guard = self.inner.lock().expect("meta mutex poisoned");
        match &self.backend {
            MetaStoreBackend::Json { .. } => {
                let path = self.state_dir.join(INSTALLATION_UUID_FILENAME);
                if path.exists() {
                    return read_installation_uuid(&path);
                }
                let uuid = uuid::Uuid::new_v4().to_string();
                crate::infra::install::atomic_write(&path, uuid.as_bytes())?;
                read_installation_uuid(&path)
            }
            MetaStoreBackend::Sqlite(state_dir) => {
                let conn = crate::state::store::open_in(state_dir)?;
                let tx = conn
                    .unchecked_transaction()
                    .map_err(|e| CaduceusError::StateCorrupt {
                        path: state_dir.join(crate::state::store::DB_FILENAME),
                        message: format!("cannot begin installation UUID transaction: {e}"),
                    })?;
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT value FROM state_meta WHERE key = 'installation_uuid'",
                        [],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|e| CaduceusError::StateCorrupt {
                        path: state_dir.join(crate::state::store::DB_FILENAME),
                        message: format!("cannot read installation UUID: {e}"),
                    })?;
                let uuid = match existing {
                    Some(value) => validate_installation_uuid(&value, state_dir)?,
                    None => {
                        let value = uuid::Uuid::new_v4().to_string();
                        tx.execute(
                            "INSERT INTO state_meta (key, value) VALUES ('installation_uuid', ?1)",
                            params![value],
                        )
                        .map_err(|e| CaduceusError::StateCorrupt {
                            path: state_dir.join(crate::state::store::DB_FILENAME),
                            message: format!("cannot persist installation UUID: {e}"),
                        })?;
                        value
                    }
                };
                tx.commit().map_err(|e| CaduceusError::StateCorrupt {
                    path: state_dir.join(crate::state::store::DB_FILENAME),
                    message: format!("cannot commit installation UUID: {e}"),
                })?;
                Ok(uuid)
            }
        }
    }
}

fn read_installation_uuid(path: &Path) -> CaduceusResult<String> {
    let value = fs::read_to_string(path).map_err(CaduceusError::Io)?;
    validate_installation_uuid(value.trim(), path)
}

fn validate_installation_uuid(value: &str, path: &Path) -> CaduceusResult<String> {
    uuid::Uuid::parse_str(value).map_err(|_| CaduceusError::StateCorrupt {
        path: path.to_path_buf(),
        message: format!("invalid installation UUID {value:?}"),
    })?;
    Ok(value.to_string())
}

/// Rate-limit observer backed by a [`MetaStore`]. Concurrent HTTP
/// responses call [`observe`] with the latest headers; the
/// observer merges the observation into the persisted metadata,
/// rejecting older observations.
#[derive(Debug)]
pub struct RateLimitObserver<'a> {
    store: &'a MetaStore,
}

impl<'a> RateLimitObserver<'a> {
    pub fn new(store: &'a MetaStore) -> Self {
        Self { store }
    }

    /// Merge *obs* into the persisted metadata. When the stored
    /// observation is newer (by `reset_at`), this call is a no-op.
    pub fn observe(&self, obs: RateLimitObservation) -> CaduceusResult<()> {
        self.store.update(|meta| {
            let dominated = meta
                .rate_limit
                .as_ref()
                .map(|existing| !obs.is_newer_than(existing))
                .unwrap_or(false);
            if dominated {
                return;
            }
            meta.rate_limit = Some(obs);
        })
    }
}

/// Outcome of the cadence / rate-limit precheck the daemon
/// runs at the start of every tick. The cadence gate answers
/// "may this tick proceed, and if not, why not?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CadenceDecision {
    /// Polling is allowed; the daemon should proceed.
    Proceed,
    /// The configured `poll_interval_seconds` has not elapsed
    /// since the last tick. The daemon exits 0 with a Cadence
    /// outcome.
    Cadence { next_allowed_at: DateTime<Utc> },
    /// A previous tick observed rate-limit exhaustion and the
    /// persisted reset time has not elapsed. The daemon exits 0
    /// with a RateLimited outcome.
    RateLimited { next_allowed_at: DateTime<Utc> },
}

impl CadenceDecision {
    pub fn is_proceed(&self) -> bool {
        matches!(self, CadenceDecision::Proceed)
    }
    pub fn tick_outcome(&self) -> Option<TickOutcome> {
        match self {
            CadenceDecision::Proceed => None,
            CadenceDecision::Cadence { .. } => Some(TickOutcome::SkippedCadence),
            CadenceDecision::RateLimited { .. } => Some(TickOutcome::RateLimited),
        }
    }
}

/// Cadence and rate-limit gate. Wraps a [`MetaStore`] and
/// exposes the read-modify-write operations the daemon runs at
/// tick boundaries. The precheck is read-only with respect to
/// the HTTP layer; the record methods persist observations.
#[derive(Debug)]
pub struct CadenceGate {
    store: MetaStore,
}

impl CadenceGate {
    /// Open the gate rooted at *state_dir*. The wrapped
    /// `MetaStore` is created on demand and reuses the
    /// existing JSON metadata file.
    pub fn open(state_dir: &Path) -> CaduceusResult<Self> {
        Ok(Self {
            store: MetaStore::open(state_dir)?,
        })
    }

    /// Wrap an existing `MetaStore`. Used by the daemon tick when
    /// the configured backend is SQLite so the gate observes the
    /// same storage as the rest of the tick.
    pub fn open_with_store(store: MetaStore) -> Self {
        Self { store }
    }

    /// Borrow the underlying metadata store (test seam).
    pub fn store(&self) -> &MetaStore {
        &self.store
    }

    /// Decide whether the tick at *now* may proceed. Pure
    /// read-only; does not persist anything. The decision is
    /// the most restrictive of:
    ///
    /// 1. The persisted rate-limit reset has not elapsed.
    /// 2. The configured `poll_interval_seconds` has not elapsed
    ///    since `last_tick_finished` (the more conservative
    ///    interpretation of "two cron invocations" the
    ///    contract calls for).
    pub fn precheck(&self, now: DateTime<Utc>, poll_interval_seconds: u64) -> CadenceDecision {
        let snapshot = self.store.snapshot();
        // Rate-limit gate: persisted reset_at in the future blocks.
        if let Some(rate) = snapshot.rate_limit.as_ref() {
            if rate.remaining == 0 && rate.reset_at > now {
                return CadenceDecision::RateLimited {
                    next_allowed_at: rate.reset_at,
                };
            }
        }
        // Cadence gate: last_tick_finished + poll_interval_seconds > now blocks.
        if let Some(last) = snapshot.last_tick_finished.as_ref() {
            let next = *last + chrono::Duration::seconds(poll_interval_seconds as i64);
            if next > now {
                return CadenceDecision::Cadence {
                    next_allowed_at: next,
                };
            }
        }
        CadenceDecision::Proceed
    }

    /// Record that a tick started at *now*. Persists
    /// `last_tick_started` so the next precheck can compute its
    /// gate.
    pub fn record_tick_started(&self, now: DateTime<Utc>) -> CaduceusResult<()> {
        self.store.update(|meta| {
            meta.last_tick_started = Some(now);
        })
    }

    /// Record the outcome of a finished tick. Persists
    /// `last_tick_finished`, `last_outcome`, `last_http_status`,
    /// and `next_allowed_poll_at` (computed from the outcome).
    /// When *rate_limit* is `Some`, the observation is persisted
    /// atomically and `next_allowed_poll_at` is set to the
    /// observation's `reset_at`; otherwise the next-allowed time
    /// is `now + poll_interval_seconds`.
    pub fn record_tick_finished(
        &self,
        now: DateTime<Utc>,
        outcome: TickOutcome,
        http_status: Option<u16>,
        poll_interval_seconds: u64,
        rate_limit: Option<&RateLimitInfo>,
        last_error: Option<String>,
    ) -> CaduceusResult<()> {
        // Persist the rate-limit observation first so the
        // snapshot taken inside `update` already sees the new
        // entry.
        let mut persisted_rate_limit: Option<RateLimitObservation> = None;
        if let Some(info) = rate_limit {
            persisted_rate_limit = Some(self.record_rate_limit(info)?);
        }
        let next_allowed_poll_at = match outcome {
            TickOutcome::RateLimited => persisted_rate_limit
                .as_ref()
                .map(|r| r.reset_at)
                .or_else(|| self.store.snapshot().rate_limit.map(|r| r.reset_at)),
            _ => Some(now + chrono::Duration::seconds(poll_interval_seconds as i64)),
        };
        self.store.update(|meta| {
            meta.last_tick_finished = Some(now);
            meta.last_outcome = Some(outcome);
            meta.last_http_status = http_status;
            if let Some(next) = next_allowed_poll_at {
                meta.next_allowed_poll_at = Some(next);
            }
            if let Some(err) = last_error {
                meta.last_error = Some(err);
            }
        })
    }

    /// Persist a [`RateLimitInfo`] from a GitHub response. The
    /// observation is only accepted when the new `reset_at` is
    /// strictly newer than the previously persisted one, per
    /// the meta-layer stale-observation rule.
    pub fn record_rate_limit(
        &self,
        info: &crate::github::RateLimitInfo,
    ) -> CaduceusResult<RateLimitObservation> {
        let now = info.observed_at;
        let reset_at = info.reset_at(now);
        let obs = RateLimitObservation {
            limit: info.limit,
            remaining: info.remaining,
            reset_at,
            observed_at: now,
        };
        RateLimitObserver::new(&self.store).observe(obs.clone())?;
        Ok(obs)
    }

    /// Persist the server-suggested poll interval. Used after
    /// the daemon has finished a tick and observed an
    /// `X-Poll-Interval` header on any response; the next
    /// precheck uses the longer of the configured and the
    /// server-suggested interval.
    pub fn record_poll_interval(
        &self,
        now: DateTime<Utc>,
        server_suggested_seconds: u64,
    ) -> CaduceusResult<()> {
        self.store.update(|meta| {
            let current = meta
                .next_allowed_poll_at
                .map(|t| (t - now).num_seconds().max(0) as u64)
                .unwrap_or(0);
            // Take the longer of the existing and the new interval
            // so the server-suggested floor only ever slows the
            // daemon down, never speeds it up.
            let effective = current.max(server_suggested_seconds);
            meta.next_allowed_poll_at = Some(now + chrono::Duration::seconds(effective as i64));
        })
    }
}

/// Read the active metadata file. Returns the parsed envelope, or
/// `StateMeta::empty()` when the file does not exist. A file that
/// exists but cannot be parsed is preserved, the corrupt marker is
/// written, and a `StateCorrupt` error is returned.
pub fn load(state_dir: &Path) -> CaduceusResult<StateMeta> {
    let meta_path = state_dir.join(META_FILENAME);
    read_active(&meta_path)
}

fn read_active(meta_path: &Path) -> CaduceusResult<StateMeta> {
    if !meta_path.exists() {
        return Ok(StateMeta::empty());
    }
    let bytes = match fs::read(meta_path) {
        Ok(b) => b,
        Err(err) => {
            quarantine_corrupt(meta_path, &err.to_string())?;
            return Err(CaduceusError::StateCorrupt {
                path: meta_path.to_path_buf(),
                message: format!("read state_meta: {err}"),
            });
        }
    };
    let parsed: Result<StateMeta, _> = serde_json::from_slice(&bytes);
    match parsed {
        Ok(meta) => {
            if meta.version != META_VERSION {
                quarantine_corrupt(
                    meta_path,
                    &format!(
                        "unsupported metadata version: got {}, expected {}",
                        meta.version, META_VERSION
                    ),
                )?;
                Err(CaduceusError::StateCorrupt {
                    path: meta_path.to_path_buf(),
                    message: format!(
                        "unsupported metadata version: got {}, expected {}",
                        meta.version, META_VERSION
                    ),
                })
            } else {
                Ok(meta)
            }
        }
        Err(err) => {
            quarantine_corrupt(meta_path, &err.to_string())?;
            Err(CaduceusError::StateCorrupt {
                path: meta_path.to_path_buf(),
                message: format!("parse state_meta: {err}"),
            })
        }
    }
}

/// Persist *meta* via the same atomic rename strategy as the queue
/// state file.
pub fn save(state_dir: &Path, meta: &StateMeta) -> CaduceusResult<()> {
    let meta_path = state_dir.join(META_FILENAME);
    save_atomic(&meta_path, meta)
}

fn save_atomic(meta_path: &Path, meta: &StateMeta) -> CaduceusResult<()> {
    if let Some(parent) = meta_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = meta_path.with_extension("json.tmp");
    let body = serde_json::to_vec(meta).map_err(|err| CaduceusError::StateCorrupt {
        path: meta_path.to_path_buf(),
        message: format!("serialize state_meta: {err}"),
    })?;
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, meta_path)?;
    Ok(())
}

/// Copy the corrupt file to a timestamped name and write the
/// corrupt marker. The original is preserved.
fn quarantine_corrupt(meta_path: &Path, reason: &str) -> CaduceusResult<()> {
    let parent = match meta_path.parent() {
        Some(p) => p.to_path_buf(),
        None => return Ok(()),
    };
    if !parent.exists() {
        fs::create_dir_all(&parent)?;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup_name = format!("{}.corrupt-{ts}", META_FILENAME);
    let backup = parent.join(backup_name);
    let _ = fs::copy(meta_path, &backup);
    let marker = parent.join(CORRUPT_MARKER_FILENAME);
    let body = format!(
        "state_meta quarantine at {ts}\nreason: {reason}\nbackup: {}\n",
        backup.display()
    );
    fs::write(&marker, body)?;
    Ok(())
}

fn take_meta_tx_conn() -> Option<Connection> {
    META_TX_CONN.with(|cell| cell.borrow_mut().take())
}

fn load_sqlite(conn: &Connection, state_dir: &Path) -> CaduceusResult<StateMeta> {
    let db_path = state_dir.join(crate::state::store::DB_FILENAME);
    let rows: Result<Vec<(String, String)>, _> = conn
        .prepare(
            "SELECT key, value FROM state_meta WHERE key LIKE 'meta_%' OR key IN
             ('version', 'last_tick_started', 'last_tick_finished', 'last_outcome',
              'last_http_status', 'next_allowed_poll_at', 'last_reap_at',
              'last_reaped_count', 'rate_limit', 'last_error', 'recent_diagnostics')",
        )
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot prepare state_meta select: {e}"),
        })?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot query state_meta: {e}"),
        })?
        .collect();
    let rows = rows.map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.clone(),
        message: format!("cannot read state_meta: {e}"),
    })?;
    if rows.is_empty() {
        return Ok(StateMeta::empty());
    }
    let mut obj = serde_json::Map::new();
    for (k, v) in rows {
        let parsed: serde_json::Value = serde_json::from_str(&v).unwrap_or(serde_json::Value::Null);
        obj.insert(k, parsed);
    }
    let value = serde_json::Value::Object(obj);
    let meta: StateMeta =
        serde_json::from_value(value).map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot decode state_meta: {e}"),
        })?;
    if meta.version != META_VERSION {
        return Err(CaduceusError::StateCorrupt {
            path: db_path,
            message: format!(
                "unsupported metadata version: got {}, expected {}",
                meta.version, META_VERSION
            ),
        });
    }
    Ok(meta)
}

fn save_sqlite(conn: &Connection, state_dir: &Path, meta: &StateMeta) -> CaduceusResult<()> {
    let db_path = state_dir.join(crate::state::store::DB_FILENAME);
    let value = serde_json::to_value(meta).map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.clone(),
        message: format!("cannot serialize state_meta: {e}"),
    })?;
    let obj = value
        .as_object()
        .ok_or_else(|| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: "state_meta serialized to non-object".to_string(),
        })?;
    for (k, v) in obj {
        let json = serde_json::to_string(v).map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot encode state_meta field {k}: {e}"),
        })?;
        conn.execute(
            "INSERT OR REPLACE INTO state_meta (key, value) VALUES (?1, ?2)",
            params![k, json],
        )
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot write state_meta key {k}: {e}"),
        })?;
    }
    Ok(())
}

/// Append a diagnostic entry to *meta*, coalescing duplicates.
pub fn append_diagnostic(
    meta: &mut StateMeta,
    code: impl Into<String>,
    issue_key: Option<IssueKey>,
    message: impl Into<String>,
) {
    let now = Utc::now();
    let code_str: String = code.into();
    let message_str: String = message.into();
    let trimmed = truncate_message(&message_str);
    let exists_recent = meta.recent_diagnostics.iter_mut().find(|d| {
        d.code == code_str
            && d.issue_key == issue_key
            && now - d.timestamp < DIAGNOSTIC_COALESCE_WINDOW
    });
    if let Some(existing) = exists_recent {
        // Refresh the timestamp and the message; the duplicate
        // does not grow the file.
        existing.timestamp = now;
        existing.message = trimmed;
        return;
    }
    meta.recent_diagnostics.push(DaemonDiagnostic {
        timestamp: now,
        code: code_str,
        issue_key,
        message: trimmed,
    });
    if meta.recent_diagnostics.len() > MAX_DIAGNOSTICS {
        let drop_count = meta.recent_diagnostics.len() - MAX_DIAGNOSTICS;
        meta.recent_diagnostics.drain(0..drop_count);
    }
}

fn truncate_message(message: &str) -> String {
    const MAX_BYTES: usize = 256;
    if message.len() <= MAX_BYTES {
        return message.to_string();
    }
    let mut end = MAX_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_string()
}

/// Convenience used by tests to produce a stable hashmap of
/// rate-limit timestamps for assertion.
#[allow(dead_code)]
pub fn rate_limit_index() -> BTreeMap<&'static str, RateLimitObservation> {
    BTreeMap::new()
}
