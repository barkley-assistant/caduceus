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

// -----------------------------------------------------------------------
// acquire_next_locked — the body of acquire_next, factored out so
// the retry-on-race path stays linear and the borrow on `state`
// stays scoped to a single iteration.
// -----------------------------------------------------------------------

pub(crate) fn acquire_next_locked(
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

pub(crate) fn write_and_sync_claim(file: &File, body: &[u8]) -> CaduceusResult<()> {
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
pub(crate) fn set_mode_0600(file: &File) -> CaduceusResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = file.metadata()?.permissions();
    perms.set_mode(0o600);
    file.set_permissions(perms)?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_mode_0600(_file: &File) -> CaduceusResult<()> {
    Ok(())
}
// -----------------------------------------------------------------------
// Free-standing helpers
// -----------------------------------------------------------------------

pub(crate) fn matches_token(entry: &QueueEntry, claim: &ClaimToken) -> bool {
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

pub(crate) fn claim_mismatch(claim: &ClaimToken) -> CaduceusError {
    CaduceusError::Queue {
        context: "claim",
        stderr: format!(
            "claim token run_id {:?} digest {} does not match any in-progress entry",
            claim.run_id, claim.digest
        ),
    }
}

pub(crate) fn into_lock_error(err: std::io::Error) -> CaduceusError {
    CaduceusError::Io(err)
}

pub(crate) fn scrub(value: &str) -> String {
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

pub(crate) fn advance_to_end_of_value(s: &str, start: usize) -> usize {
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

pub(crate) fn atomic_write(target: &Path, body: &[u8]) -> CaduceusResult<()> {
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

pub(crate) fn sync_dir(dir: &Path) -> CaduceusResult<()> {
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

pub fn unlink_claim_best_effort(claims_dir: &Path, claim: &ClaimToken) {
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

pub(crate) fn update_claim_worktree(
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
