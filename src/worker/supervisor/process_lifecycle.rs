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
// Hidden command name
// ---------------------------------------------------------------------------

/// Hidden command name that the binary recognises before public
/// Clap parsing. The token is reserved and must never appear in
/// `--help` output or be accepted from cron / plugin
/// configuration.
pub const HIDDEN_COMMAND: &str = "__worker-supervisor";

/// Current protocol version. Bumped together with the framing
/// rules — the daemon and supervisor reject any frame whose
/// version does not match.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum size of any single framed message (control +
/// payload). Bound chosen to fit inside a single `write(2)`
/// on every Unix we support while leaving room for the
/// envelope.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
// ---------------------------------------------------------------------------
// Subreaper + setsid + signal helpers (safe wrappers via nix)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub(crate) fn try_set_subreaper() -> CaduceusResult<()> {
    nix::sys::prctl::set_child_subreaper(true).map_err(|err| CaduceusError::Worker {
        context: "supervisor:subreaper",
        stderr: format!("prctl(PR_SET_CHILD_SUBREAPER) failed: {err}"),
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn try_set_subreaper() -> CaduceusResult<()> {
    Ok(())
}

/// `setsid` the calling process into a new session.
pub fn detach_session() -> CaduceusResult<()> {
    nix::unistd::setsid().map_err(|err| CaduceusError::Worker {
        context: "supervisor:setsid",
        stderr: format!("setsid: {err}"),
    })?;
    Ok(())
}

/// Walk `/proc` for every PID whose `stat` reports our PID
/// (or another tracked PID) as its parent. On non-Linux
/// platforms this returns an empty list; the caller falls
/// back to the worker process-group kill path.
#[cfg(target_os = "linux")]
pub fn collect_descendants(ppid: i32) -> Vec<i32> {
    use std::fs;
    let mut out = Vec::new();
    let entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if pid == ppid {
            continue;
        }
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Some(p) = parse_stat_parent(&stat) {
            if p == ppid {
                out.push(pid);
            }
        }
    }
    out
}

/// Best-effort parser for `/proc/<pid>/stat`.
pub(crate) fn parse_stat_parent(stat: &str) -> Option<i32> {
    let close = stat.rfind(')')?;
    let after = &stat[close + 1..];
    let mut it = after.split_whitespace();
    let _state = it.next()?;
    let ppid: i32 = it.next()?.parse().ok()?;
    Some(ppid)
}

// ---------------------------------------------------------------------------
// Process-identity helpers — read /proc/<pid>/stat starttime to detect PID
// reuse before signalling. Used by the deadline-enforcement machinery in
// later work units.
// ---------------------------------------------------------------------------

/// Parse field 22 (starttime in clock ticks) from a `/proc/<pid>/stat`
/// string. Returns `None` if the line is malformed.
///
/// Per `proc(5)`, the stat line is `pid (comm) state ppid ... starttime ...`
/// where `starttime` is the 22nd field overall. After the `)`, `state` is the
/// first token (field 3), so `starttime` lands at after-paren index 19.
#[cfg(target_os = "linux")]
pub(crate) fn parse_starttime_from_stat(stat: &str) -> Option<u64> {
    let after_paren = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_paren.split_whitespace().collect();
    let starttime = fields.get(19).copied()?;
    starttime.parse::<u64>().ok()
}

/// Read process starttime in clock ticks from `/proc/<pid>/stat`,
/// field 22.  Returns `None` if the process no longer exists or the
/// stat file cannot be read.
#[cfg(target_os = "linux")]
pub fn read_proc_starttime(pid: i32) -> Option<u64> {
    use std::fs;
    let body = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_starttime_from_stat(&body)
}

/// Return `true` only when *pid* still refers to the same process
/// incarnation whose starttime was *expected_starttime*.  Returns
/// `false` if the process has exited (PID recycled) or the starttime
/// differs (PID reuse).
#[cfg(target_os = "linux")]
pub fn verify_identity(pid: i32, expected_starttime: u64) -> bool {
    read_proc_starttime(pid) == Some(expected_starttime)
}

#[cfg(not(target_os = "linux"))]
pub fn read_proc_starttime(_pid: i32) -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
pub fn verify_identity(_pid: i32, _expected: u64) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
pub fn collect_descendants(_ppid: i32) -> Vec<i32> {
    Vec::new()
}

/// Send *signal* to *pid*. Errors are intentionally swallowed.
#[cfg(unix)]
pub fn kill_pid(pid: i32, signal: i32) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let Ok(sig) = Signal::try_from(signal) else {
        return;
    };
    let _ = kill(Pid::from_raw(pid), sig);
}

/// Send *signal* to the process group with the given negative
/// PGID. Used to broadcast SIGTERM / SIGKILL to the whole
/// worker session.
#[cfg(unix)]
pub fn kill_pgid(pgid: i32, signal: i32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    let Ok(sig) = Signal::try_from(signal) else {
        return;
    };
    let _ = killpg(Pid::from_raw(pgid), sig);
}

// ---------------------------------------------------------------------------
// Hidden command dispatch + env construction
// ---------------------------------------------------------------------------

/// Build the `caduceus __worker-supervisor` command for *args*.
/// The hidden command is dispatched before Clap parsing so it
/// is never shown in `--help` output and is never accepted
/// from cron / plugin configuration. The supervisor only sees
/// the cleared worker environment; no daemon credentials
/// reach it.
///
/// The daemon-side uses `Child::stdin/stdout/stderr` for the
/// control/status pipes — the supervisor inherits them as
/// the canonical "inherited file descriptors" the contract
/// requires.
#[allow(clippy::too_many_arguments)]
pub fn build_supervisor_command(
    self_exe: &Path,
    worktree: &Path,
    run_id: &str,
    issue: &IssueKey,
    context_json: &str,
    worker_command: &[String],
    transcript_path: &Path,
    heartbeat_path: &Path,
    timeout_seconds: u64,
    transcript_max_bytes: u64,
) -> Command {
    let mut cmd = Command::new(self_exe);
    cmd.arg(HIDDEN_COMMAND);
    cmd.arg("--worktree").arg(worktree);
    cmd.arg("--run-id").arg(run_id);
    cmd.arg("--issue")
        .arg(format!("{}/{}#{}", issue.owner, issue.repo, issue.number));
    cmd.arg("--context-json").arg(context_json);
    cmd.arg("--transcript").arg(transcript_path);
    cmd.arg("--heartbeat").arg(heartbeat_path);
    cmd.arg("--timeout").arg(timeout_seconds.to_string());
    cmd.arg("--transcript-max-bytes")
        .arg(transcript_max_bytes.to_string());
    for arg in worker_command {
        cmd.arg("--").arg(arg);
    }
    // The supervisor's stdin/stdout/stderr are the daemon's
    // control/status pipes. Stderr is captured separately so a
    // misbehaving supervisor can log to disk.
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.env_clear();
    cmd
}
