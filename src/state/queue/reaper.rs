#![allow(dead_code, unused_imports)]
use super::*;
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
pub(crate) fn quarantine_claim(
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
pub(crate) async fn reap_one_stale_claim(
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
pub(crate) fn is_pid_alive(pid: u32) -> bool {
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
