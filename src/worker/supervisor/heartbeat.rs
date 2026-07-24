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
// Heartbeat
// ---------------------------------------------------------------------------

/// The versioned JSON envelope the supervisor writes to a
/// `<state_dir>/runs/<run_id>.heartbeat` file. The contract
/// pins the field set; `status` reads the same envelope to
/// surface a live worker. The on-disk encoding is one
/// pretty-printed JSON object terminated by `\n`; the
/// supervisor rewrites the file atomically (`O_CREAT |
/// O_TRUNC | O_NOFOLLOW` then `sync_all` then `rename`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub version: u32,
    pub run_id: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub issue_key: IssueKey,
    pub transcript_path: PathBuf,
}

/// File-format version the supervisor writes. The first
/// versioned shape; older unversioned timestamp-only
/// heartbeats are not recognised by `status` and are
/// surfaced as `HeartbeatParseError::Malformed` so the
/// operator can investigate.
pub const HEARTBEAT_FILE_VERSION: u32 = 1;

/// Write the heartbeat file atomically. The supervisor
/// calls this at most once per second while the worker is
/// alive; `status` reads the file and considers it stale
/// after 90 seconds.
pub fn write_heartbeat_record(record: &Heartbeat, path: &Path) -> CaduceusResult<()> {
    let json = serde_json::to_string_pretty(record).map_err(|err| CaduceusError::Worker {
        context: "supervisor:heartbeat",
        stderr: format!("serialize heartbeat: {err}"),
    })?;
    let tmp = path.with_extension("heartbeat.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:heartbeat",
            stderr: format!("open {}: {err}", tmp.display()),
        })?;
    file.write_all(json.as_bytes())
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:heartbeat",
            stderr: format!("write {}: {err}", tmp.display()),
        })?;
    file.write_all(b"\n").ok();
    file.sync_all().ok();
    set_mode(&tmp, 0o600)?;
    std::fs::rename(&tmp, path).map_err(|err| CaduceusError::Worker {
        context: "supervisor:heartbeat",
        stderr: format!("rename {} -> {}: {err}", tmp.display(), path.display()),
    })?;
    set_mode(path, 0o600)?;
    Ok(())
}

/// Backwards-compatible wrapper that writes the
/// versioned envelope using the supervisor's current
/// time as both `started_at` and `updated_at`. Tests that
/// only need a fresh `updated_at` (e.g. supervisor unit
/// tests) keep using this entry point; production paths
/// construct a [`Heartbeat`] explicitly so the daemon
/// can distinguish "first write" from "subsequent write".
pub fn write_heartbeat(path: &Path) -> CaduceusResult<()> {
    let run_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    let now = Utc::now();
    let record = Heartbeat {
        version: HEARTBEAT_FILE_VERSION,
        run_id,
        pid: std::process::id(),
        started_at: now,
        updated_at: now,
        issue_key: IssueKey {
            owner: String::new(),
            repo: String::new(),
            number: 0,
        },
        transcript_path: path.with_extension("log"),
    };
    write_heartbeat_record(&record, path)
}

/// Remove the heartbeat file once the supervisor returns.
pub fn clear_heartbeat(path: &Path) -> CaduceusResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(CaduceusError::Worker {
            context: "supervisor:heartbeat",
            stderr: format!("remove {}: {err}", path.display()),
        }),
    }
}

/// Read the heartbeat file's `updated_at` timestamp. The
/// helper accepts both the versioned JSON envelope and the
/// legacy unversioned RFC 3339 timestamp so older files
/// stay readable during the rollout. Returns `None` for a
/// missing or malformed file.
pub fn read_heartbeat(path: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    read_heartbeat_record(path).map(|r| r.updated_at)
}

/// Read the heartbeat file as the full [`Heartbeat`]
/// envelope. Returns `None` for a missing or malformed
/// file. Used by `status` to surface the live worker.
pub fn read_heartbeat_record(path: &Path) -> Option<Heartbeat> {
    let mut file = File::open(path).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    if let Ok(record) = serde_json::from_str::<Heartbeat>(&buf) {
        if record.version == HEARTBEAT_FILE_VERSION {
            return Some(record);
        }
        return None;
    }
    // Legacy format: a single RFC 3339 line. We synthesise
    // a v1 envelope so the rest of the status surface can
    // treat heartbeats uniformly.
    let updated_at = chrono::DateTime::parse_from_rfc3339(buf.trim())
        .ok()?
        .with_timezone(&chrono::Utc);
    let run_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    Some(Heartbeat {
        version: HEARTBEAT_FILE_VERSION,
        run_id,
        pid: 0,
        started_at: updated_at,
        updated_at,
        issue_key: IssueKey {
            owner: String::new(),
            repo: String::new(),
            number: 0,
        },
        transcript_path: path.with_extension("log"),
    })
}
