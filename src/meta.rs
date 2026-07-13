//! `<state_dir>/state_meta.json` schema, `MetaStore`, and the
//! crash-safe writer. This module owns the canonical metadata
//! envelope and the `RateLimitObserver` defined here so the
//! HTTP layer (Phase 2) and the persistence layer (this phase)
//! can evolve independently.
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

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{CaduceusError, CaduceusResult};
use crate::issue::IssueKey;

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

/// Persisted tick metadata. Field semantics are pinned by
/// `CONTRACTS.md` under "State metadata and status".
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
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TickOutcome {
    Processed,
    Idle,
    Concurrent,
    Cadence,
    RateLimited,
    Cancelled,
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

/// One diagnostic entry. Field semantics are pinned by
/// `CONTRACTS.md`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonDiagnostic {
    pub timestamp: DateTime<Utc>,
    pub code: String,
    pub issue_key: Option<IssueKey>,
    pub message: String,
}

/// Read-modify-write store for `StateMeta`. The mutex serialises
/// concurrent updates so concurrent HTTP responses can merge
/// rate-limit observations without losing fields.
#[derive(Debug)]
pub struct MetaStore {
    state_dir: PathBuf,
    meta_path: PathBuf,
    corrupt_marker: PathBuf,
    inner: Mutex<StateMeta>,
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
            meta_path,
            corrupt_marker,
            inner: Mutex::new(meta),
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
        save_atomic(&self.meta_path, &guard)
    }

    /// Borrow the metadata without modifying it.
    pub fn snapshot(&self) -> StateMeta {
        self.inner.lock().expect("meta mutex poisoned").clone()
    }

    /// True when the corrupt marker is present.
    pub fn is_corrupt(&self) -> bool {
        self.corrupt_marker.exists()
    }

    /// Clear the corrupt marker after a successful recovery.
    pub fn clear_corrupt_marker(&self) -> CaduceusResult<()> {
        if self.corrupt_marker.exists() {
            fs::remove_file(&self.corrupt_marker)?;
        }
        Ok(())
    }

    /// Path to the corrupt marker (test seam).
    pub fn corrupt_marker_path(&self) -> &Path {
        &self.corrupt_marker
    }

    /// Path to the active metadata file (test seam).
    pub fn meta_path(&self) -> &Path {
        &self.meta_path
    }

    /// The state directory (test seam).
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
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

/// Convenience used by tests and by future Phase 7 callers to
/// produce a stable hashmap of rate-limit timestamps for assertion.
#[allow(dead_code)]
pub fn rate_limit_index() -> BTreeMap<&'static str, RateLimitObservation> {
    BTreeMap::new()
}
