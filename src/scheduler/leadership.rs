//! Leader-election primitive.
//!
//! [`LeaderToken`] is acquired via an OS-level exclusive file lock
//! on `<state_dir>/scheduler.lock`. The lock is held only during
//! short state-mutation transactions, not for the whole tick.

use std::fs::{File, OpenOptions};
use std::path::Path;

use fs2::FileExt;

use crate::infra::error::{CaduceusError, CaduceusResult};

/// The scheduler lock filename inside the state directory.
const SCHEDULER_LOCK_FILENAME: &str = "scheduler.lock";

/// A token proving the caller holds the scheduler leadership lock.
/// Dropping the token releases the lock.
#[derive(Debug)]
pub struct LeaderToken {
    _file: File,
}

impl LeaderToken {
    /// Try to acquire the scheduler leadership lock. Returns
    /// `Ok(Some(token))` on success, `Ok(None)` if the lock is
    /// contended, or `Err` on I/O errors.
    pub fn try_acquire(state_dir: &Path) -> CaduceusResult<Option<Self>> {
        let lock_path = state_dir.join(SCHEDULER_LOCK_FILENAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| CaduceusError::LeadershipContended {
                context: "try_acquire",
                stderr: format!("open lock file: {e}"),
            })?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(CaduceusError::LeadershipContended {
                context: "try_lock_exclusive",
                stderr: format!("{err}"),
            }),
        }
    }

    /// Run a closure inside the scheduler leadership transaction.
    /// The leadership lock is held for the duration of the closure.
    /// The lock is released when the closure returns.
    pub fn with_lock<T>(
        state_dir: &Path,
        f: impl FnOnce() -> CaduceusResult<T>,
    ) -> CaduceusResult<T> {
        let _token = match Self::try_acquire(state_dir)? {
            Some(token) => token,
            None => {
                return Err(CaduceusError::LeadershipContended {
                    context: "with_lock",
                    stderr: "leadership lock contended".to_string(),
                });
            }
        };
        f()
    }
}
