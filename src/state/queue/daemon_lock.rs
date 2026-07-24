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
