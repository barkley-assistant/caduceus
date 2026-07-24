#![allow(dead_code, unused_imports)]
use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command as TokioCommand};

use crate::github::issue::IssueKey;
use crate::infra::error::{CaduceusError, CaduceusResult};

// ---------------------------------------------------------------------------
// Worker outcome + transcript
// ---------------------------------------------------------------------------

/// Outcome the daemon sees when the supervisor returns. The
/// daemon's `spawn` returns this to the orchestration loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupervisorOutcome {
    /// The bridge exit code when it exited normally.
    pub status: i32,
    /// True when the supervisor killed the bridge by signal.
    pub signaled: bool,
    /// True when the supervisor hit the configured worker
    /// timeout and killed the worker session.
    pub timed_out: bool,
    /// True when the daemon asked for cancellation (timeout,
    /// SIGINT, or SIGTERM) and the supervisor confirmed the
    /// worker session is gone.
    pub cancelled: bool,
}

/// Path layout for one worker's runtime artefacts. The
/// supervisor writes the transcript here; the daemon reads the
/// result file once the supervisor returns.
#[derive(Clone, Debug)]
pub struct WorkerRunPaths {
    pub state_dir: PathBuf,
    pub run_id: String,
    pub transcript_path: PathBuf,
    pub heartbeat_path: PathBuf,
    pub result_path: PathBuf,
}

impl WorkerRunPaths {
    pub fn new(state_dir: PathBuf, run_id: String) -> Self {
        let runs = state_dir.join("runs");
        let transcript_path = runs.join(format!("{run_id}.log"));
        let heartbeat_path = runs.join(format!("{run_id}.heartbeat"));
        let result_path = runs.join(format!("{run_id}.result.json"));
        Self {
            state_dir,
            run_id,
            transcript_path,
            heartbeat_path,
            result_path,
        }
    }

    /// Ensure the parent directories exist with the documented
    /// secure mode (`0700`).
    pub fn ensure_dirs(&self) -> CaduceusResult<()> {
        let runs = self.state_dir.join("runs");
        fs::create_dir_all(&runs).map_err(|err| CaduceusError::Worker {
            context: "supervisor:setup",
            stderr: format!("create {}: {err}", runs.display()),
        })?;
        set_mode(&runs, 0o700)?;
        Ok(())
    }
}

/// Create the transcript file with `0600` mode. Returns the
/// opened `File` for the writer task.
pub fn open_transcript(path: &Path) -> CaduceusResult<File> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:transcript",
            stderr: format!("open {}: {err}", path.display()),
        })?;
    set_mode(path, 0o600)?;
    Ok(file)
}

/// Append *bytes* to *file*. Errors surface as a
/// `CaduceusError::Worker { context: "transcript", stderr }`.
pub fn append_transcript(file: &mut File, bytes: &[u8]) -> CaduceusResult<()> {
    file.write_all(bytes).map_err(|err| CaduceusError::Worker {
        context: "supervisor:transcript",
        stderr: format!("write transcript: {err}"),
    })
}

/// Truncate the transcript to *max_bytes* and append the
/// documented `...<truncated N bytes>` marker so the tail is
/// still readable. The function is a no-op when the file is
/// already short enough.
pub fn truncate_transcript(path: &Path, max_bytes: u64) -> CaduceusResult<bool> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(false);
    };
    if meta.file_type().is_symlink() {
        return Err(CaduceusError::Worker {
            context: "supervisor:transcript",
            stderr: format!("refusing to follow symlink at {}", path.display()),
        });
    }
    if meta.len() <= max_bytes {
        return Ok(false);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:transcript",
            stderr: format!("reopen {}: {err}", path.display()),
        })?;
    let mut kept = Vec::with_capacity(max_bytes as usize);
    file.take(max_bytes)
        .read_to_end(&mut kept)
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:transcript",
            stderr: format!("read transcript: {err}"),
        })?;
    let marker = format!(
        "\n...<truncated {} bytes>\n",
        meta.len().saturating_sub(max_bytes)
    );
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:transcript",
            stderr: format!("reopen for truncate {}: {err}", path.display()),
        })?;
    file.write_all(&kept).map_err(|err| CaduceusError::Worker {
        context: "supervisor:transcript",
        stderr: format!("write kept: {err}"),
    })?;
    file.write_all(marker.as_bytes())
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:transcript",
            stderr: format!("write marker: {err}"),
        })?;
    file.sync_all().ok();
    set_mode(path, 0o600)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// BoundedTranscriptWriter — bounded stderr capture
// ---------------------------------------------------------------------------

/// Bounded writer that wraps a transcript file with a byte cap.
/// Writes that would exceed the cap trigger truncation; subsequent
/// writes are still appended (to keep the drain running) but the
/// truncated flag is set. On `finalize()`, reports truncation or
/// write failures as an error so the caller can surface them.
#[derive(Debug)]
pub struct BoundedTranscriptWriter {
    pub file: File,
    path: PathBuf,
    max_bytes: u64,
    pub truncated: bool,
    pub write_failures: u64,
}

impl BoundedTranscriptWriter {
    /// Create a new bounded writer. Opens the transcript file via
    /// [`open_transcript`]; errors propagate.
    pub fn new(path: PathBuf, max_bytes: u64) -> CaduceusResult<Self> {
        let file = open_transcript(&path)?;
        Ok(Self {
            file,
            path,
            max_bytes,
            truncated: false,
            write_failures: 0,
        })
    }

    /// Append *bytes* to the transcript. On I/O error, increments
    /// `write_failures` and returns (does NOT propagate — the drain
    /// must keep running). After a successful append, checks the file
    /// size; if it exceeds `max_bytes`, calls [`truncate_transcript`]
    /// and sets `truncated = true`.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        if append_transcript(&mut self.file, bytes).is_err() {
            self.write_failures += 1;
            return;
        }
        let _ = self.file.flush();
        if !self.truncated {
            if let Ok(meta) = fs::symlink_metadata(&self.path) {
                if meta.len() > self.max_bytes {
                    if let Ok(true) = truncate_transcript(&self.path, self.max_bytes) {
                        self.truncated = true;
                    }
                }
            }
        }
    }

    /// Finalize the transcript. Returns:
    /// - `Err(CaduceusError::Worker { context: "supervisor:transcript:truncated", .. })`
    ///   if the transcript was truncated (takes precedence).
    /// - `Err(CaduceusError::Worker { context: "supervisor:transcript:write_failures", .. })`
    ///   if there were write failures.
    /// - `Ok(())` otherwise.
    pub fn finalize(self) -> CaduceusResult<()> {
        if self.truncated {
            return Err(CaduceusError::Worker {
                context: "supervisor:transcript:truncated",
                stderr: format!("transcript truncated at {} bytes", self.max_bytes),
            });
        }
        if self.write_failures > 0 {
            return Err(CaduceusError::Worker {
                context: "supervisor:transcript:write_failures",
                stderr: format!("{} write failure(s)", self.write_failures),
            });
        }
        Ok(())
    }
}

pub(crate) fn set_mode(path: &Path, mode: u32) -> CaduceusResult<()> {
    let meta = fs::symlink_metadata(path).map_err(|err| CaduceusError::Worker {
        context: "supervisor:setup",
        stderr: format!("stat {}: {err}", path.display()),
    })?;
    if meta.file_type().is_symlink() {
        return Err(CaduceusError::Worker {
            context: "supervisor:setup",
            stderr: format!("refusing to follow symlink at {}", path.display()),
        });
    }
    let mut perms = meta.permissions();
    perms.set_mode(mode);
    fs::set_permissions(path, perms).map_err(|err| CaduceusError::Worker {
        context: "supervisor:setup",
        stderr: format!("chmod {mode:o} {}: {err}", path.display()),
    })?;
    Ok(())
}
