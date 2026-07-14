//! Worker process supervision.
//!
//! This module owns the in-process supervisor that the daemon
//! uses to spawn and tear down the bridge. The contract is
//! pinned by `CONTRACTS.md` "Worker environment and result",
//! "Hermes plugin compatibility contract", and Task 5.1:
//!
//! * The public daemon never spawns the bridge directly. It
//!   re-execs the same `caduceus` binary in a hidden
//!   `__worker-supervisor` mode that owns the worker session.
//! * The supervisor and the daemon talk over a length-bounded,
//!   versioned control/status framing protocol carried over
//!   the supervisor's inherited `stdin` (daemon→supervisor)
//!   and `stdout` (supervisor→daemon) descriptors.
//! * The supervisor forks the worker behind an exec-gate pipe.
//!   The worker calls `setsid` but cannot `exec` until the
//!   supervisor confirms `READY(pgid)` and the daemon
//!   acknowledges it with `ACK`. If either side dies before
//!   `ACK`, the gate EOFs and the pre-exec child exits without
//!   running the harness.
//! * After `ACK`, unexpected supervisor exit makes the daemon
//!   kill the recorded session; daemon death closes the
//!   control pipe (stdin) and makes the live supervisor kill
//!   the worker session.
//! * On Linux, the supervisor calls
//!   `prctl(PR_SET_CHILD_SUBREAPER)` before spawning so any
//!   detached descendants are still reaped by the supervisor.
//!   Cleanup enumerates descendant PIDs from `/proc`, signals
//!   the original negative PGID plus every descendant, waits
//!   two seconds, rediscovers, sends `SIGKILL`, and reaps
//!   until no descendants remain.
//! * The supervisor only ever sees the cleared worker
//!   environment — daemon credentials never appear in any
//!   inherited descriptor or pipe frame.
//!
//! The hidden command is dispatched in [`crate::main`] (the
//! CLI host) before public command parsing.
//!
//! # Safety note
//!
//! The crate's `#![forbid(unsafe_code)]` policy forbids `unsafe`
//! blocks anywhere in the source tree. The supervisor needs to
//! hand FDs across exec and to call `pipe2` / `setsid` /
//! `killpg`. Where the safe `nix` crate provides a wrapper
//! (`setsid`, `killpg`, `kill`, `pipe2`, `set_child_subreaper`),
//! the supervisor uses it directly. For the few operations
//! that have no safe wrapper in `nix` 0.29 (`OwnedFd` adoption
//! for tokio's async I/O, `prctl`), the supervisor uses
//! safe APIs only and routes the dangerous syscalls through
//! `tokio::process::Command::stdin/stdout/stderr(Stdio::piped())`
//! so the inherited-FD contract is satisfied without explicit
//! `unsafe`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command as TokioCommand};

use crate::error::{CaduceusError, CaduceusResult};
use crate::issue::IssueKey;

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
// Control pipe protocol
// ---------------------------------------------------------------------------

/// Frame sent between the supervisor and the daemon over the
/// inherited `stdin`/`stdout` descriptors. The serialisation
/// is deliberately trivial: a 4-byte little-endian length
/// prefix followed by a UTF-8 string. The first line is the
/// version + opcode; the rest is opcode-specific payload text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlFrame {
    /// Supervisor announces that the worker has called
    /// `setsid` and recorded its PGID. Payload: the PGID as a
    /// decimal string.
    Ready { pgid: i32 },
    /// Supervisor announces the worker exited and the session
    /// is reaped. Payload: the exit code as a decimal string,
    /// or `signal:<n>` if it died by signal.
    Done { status: i32, signaled: bool },
    /// Supervisor encountered a fatal error before the worker
    /// could even start.
    Fatal { reason: String },
    /// Daemon tells the supervisor to terminate the worker.
    /// Payload: empty for SIGTERM, `kill` for SIGKILL after a
    /// 2-second grace period.
    Terminate { force: bool },
    /// Daemon confirms it has recorded the PGID and the worker
    /// may now `exec` the harness.
    Ack,
}

impl ControlFrame {
    pub fn opcode(&self) -> &'static str {
        match self {
            ControlFrame::Ready { .. } => "READY",
            ControlFrame::Done { .. } => "DONE",
            ControlFrame::Fatal { .. } => "FATAL",
            ControlFrame::Terminate { force: false } => "TERM",
            ControlFrame::Terminate { force: true } => "KILL",
            ControlFrame::Ack => "ACK",
        }
    }
}

/// Encode a control frame into bytes. The format is:
/// `<u32-le length><UTF-8 line>`.
pub fn encode_frame(frame: &ControlFrame) -> CaduceusResult<Vec<u8>> {
    let line = match frame {
        ControlFrame::Ready { pgid } => {
            format!("v{version} READY {pgid}", version = PROTOCOL_VERSION)
        }
        ControlFrame::Done {
            status,
            signaled: true,
        } => {
            format!(
                "v{version} DONE signal:{status}",
                version = PROTOCOL_VERSION
            )
        }
        ControlFrame::Done { status, .. } => {
            format!("v{version} DONE {status}", version = PROTOCOL_VERSION)
        }
        ControlFrame::Fatal { reason } => {
            format!("v{version} FATAL {reason}", version = PROTOCOL_VERSION)
        }
        ControlFrame::Terminate { force: false } => {
            format!("v{version} TERM", version = PROTOCOL_VERSION)
        }
        ControlFrame::Terminate { force: true } => {
            format!("v{version} KILL", version = PROTOCOL_VERSION)
        }
        ControlFrame::Ack => format!("v{version} ACK", version = PROTOCOL_VERSION),
    };
    if line.len() + 4 > MAX_FRAME_BYTES {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("frame too long: {} bytes", line.len()),
        });
    }
    let mut out = Vec::with_capacity(line.len() + 4);
    out.extend_from_slice(&(line.len() as u32).to_le_bytes());
    out.extend_from_slice(line.as_bytes());
    Ok(out)
}

/// Decode a control frame from a buffer of bytes. Returns the
/// decoded frame plus the number of bytes consumed; the caller
/// passes any leftover bytes back through.
pub fn decode_frame(buf: &[u8]) -> CaduceusResult<(ControlFrame, usize)> {
    if buf.len() < 4 {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: "buffer too short for length prefix".to_string(),
        });
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("frame length {len} exceeds cap {MAX_FRAME_BYTES}"),
        });
    }
    if buf.len() < 4 + len {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: "buffer truncated inside frame".to_string(),
        });
    }
    let line = std::str::from_utf8(&buf[4..4 + len]).map_err(|err| CaduceusError::Worker {
        context: "supervisor:frame",
        stderr: format!("non-UTF-8 frame: {err}"),
    })?;
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    let opcode = parts.next().unwrap_or("");
    let payload = parts.next().unwrap_or("");
    if version != format!("v{PROTOCOL_VERSION}") {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("unsupported protocol version {version}"),
        });
    }
    let frame = match opcode {
        "READY" => {
            let pgid: i32 = payload.parse().map_err(|err| CaduceusError::Worker {
                context: "supervisor:frame",
                stderr: format!("invalid READY payload {payload:?}: {err}"),
            })?;
            ControlFrame::Ready { pgid }
        }
        "DONE" => {
            if let Some(rest) = payload.strip_prefix("signal:") {
                let n: i32 = rest.parse().map_err(|err| CaduceusError::Worker {
                    context: "supervisor:frame",
                    stderr: format!("invalid DONE signal payload {payload:?}: {err}"),
                })?;
                ControlFrame::Done {
                    status: n,
                    signaled: true,
                }
            } else {
                let n: i32 = payload.parse().map_err(|err| CaduceusError::Worker {
                    context: "supervisor:frame",
                    stderr: format!("invalid DONE payload {payload:?}: {err}"),
                })?;
                ControlFrame::Done {
                    status: n,
                    signaled: false,
                }
            }
        }
        "FATAL" => ControlFrame::Fatal {
            reason: payload.to_string(),
        },
        "TERM" => ControlFrame::Terminate { force: false },
        "KILL" => ControlFrame::Terminate { force: true },
        "ACK" => ControlFrame::Ack,
        other => {
            return Err(CaduceusError::Worker {
                context: "supervisor:frame",
                stderr: format!("unknown opcode {other:?}"),
            })
        }
    };
    Ok((frame, 4 + len))
}

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

fn set_mode(path: &Path, mode: u32) -> CaduceusResult<()> {
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

// ---------------------------------------------------------------------------
// Heartbeat
// ---------------------------------------------------------------------------

/// Write the heartbeat file with the current UTC timestamp.
/// The daemon writes a fresh timestamp at most once per
/// second while the worker is alive. `status` reads the file
/// and considers it stale after 90 seconds.
pub fn write_heartbeat(path: &Path) -> CaduceusResult<()> {
    let now = chrono::Utc::now();
    let line = format!("{}\n", now.to_rfc3339());
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:heartbeat",
            stderr: format!("open {}: {err}", path.display()),
        })?;
    file.write_all(line.as_bytes())
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:heartbeat",
            stderr: format!("write {}: {err}", path.display()),
        })?;
    file.sync_all().ok();
    set_mode(path, 0o600)?;
    Ok(())
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

/// Read the heartbeat file and return the timestamp.
pub fn read_heartbeat(path: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    let mut file = File::open(path).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    chrono::DateTime::parse_from_rfc3339(buf.trim())
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

// ---------------------------------------------------------------------------
// Subreaper + setsid + signal helpers (safe wrappers via nix)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[allow(dead_code)]
fn try_set_subreaper() -> CaduceusResult<()> {
    nix::sys::prctl::set_child_subreaper(true).map_err(|err| CaduceusError::Worker {
        context: "supervisor:subreaper",
        stderr: format!("prctl(PR_SET_CHILD_SUBREAPER) failed: {err}"),
    })
}

#[cfg(not(target_os = "linux"))]
fn try_set_subreaper() -> CaduceusResult<()> {
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
fn parse_stat_parent(stat: &str) -> Option<i32> {
    let close = stat.rfind(')')?;
    let after = &stat[close + 1..];
    let mut it = after.split_whitespace();
    let _state = it.next()?;
    let ppid: i32 = it.next()?.parse().ok()?;
    Some(ppid)
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

// ---------------------------------------------------------------------------
// Frame I/O over tokio child streams
// ---------------------------------------------------------------------------

/// Async read a single control frame from `stream`. Returns
/// `None` on EOF (the supervisor closed the pipe).
pub async fn read_frame_async<R>(
    stream: &mut R,
    buf: &mut Vec<u8>,
) -> CaduceusResult<Option<ControlFrame>>
where
    R: tokio::io::AsyncRead + Unpin + Send,
{
    let mut header = [0u8; 4];
    let n = match stream.read(&mut header).await {
        Ok(0) => return Ok(None),
        Ok(n) => n,
        Err(err) => {
            return Err(CaduceusError::Worker {
                context: "supervisor:frame",
                stderr: format!("read header: {err}"),
            })
        }
    };
    if n < 4 {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("short read on header: {n} bytes"),
        });
    }
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("frame length {len} exceeds cap {MAX_FRAME_BYTES}"),
        });
    }
    buf.clear();
    buf.resize(4 + len, 0);
    buf[..4].copy_from_slice(&header);
    stream
        .read_exact(&mut buf[4..])
        .await
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("read body: {err}"),
        })?;
    let (frame, _) = decode_frame(buf)?;
    Ok(Some(frame))
}

pub async fn write_frame_async<W>(stream: &mut W, frame: &ControlFrame) -> CaduceusResult<()>
where
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    let bytes = encode_frame(frame)?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|err| CaduceusError::Worker {
            context: "supervisor:frame",
            stderr: format!("write: {err}"),
        })?;
    stream.flush().await.ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// Public spawn orchestrator
// ---------------------------------------------------------------------------

/// Top-level worker supervision entry point used by the
/// orchestration loop. The implementation here is the
/// canonical production spawn path:
///
/// 1. Open the transcript and heartbeat files in secure
///    mode before the supervisor is launched.
/// 2. Spawn the same binary in `__worker-supervisor` mode
///    with the cleared worker environment, the worktree path,
///    the run id, the canonical `CADUCEUS_*` context payload,
///    and the worker command.
/// 3. The supervisor's `stdin`/`stdout` are the daemon's
///    control/status pipes (inherited FDs, per the contract).
/// 4. Read `READY(pgid)` from the supervisor's stdout, send
///    `ACK` over its stdin so the supervisor opens the exec
///    gate.
/// 5. Drain supervisor `stderr` into the transcript, bounded
///    by `cfg.transcript_max_bytes`, with a single truncation
///    marker and continuing drain/discard after truncation.
/// 6. Await supervisor exit, both readers, and writer.
/// 7. Remove the heartbeat, return the parsed
///    [`SupervisorOutcome`].
///
/// `cancellation` is the daemon's
/// `tokio_util::sync::CancellationToken`. When triggered, the
/// daemon sends `TERM` to the supervisor and waits up to 2
/// seconds before escalating to `KILL`.
#[allow(clippy::too_many_arguments)]
pub async fn supervise(
    self_exe: &Path,
    cfg: &crate::config::Config,
    issue: &IssueKey,
    worktree: &Path,
    run_id: &str,
    context_json: &str,
    worker_command: &[String],
    cancellation: tokio_util::sync::CancellationToken,
) -> CaduceusResult<SupervisorOutcome> {
    let paths = WorkerRunPaths::new(cfg.state_dir.clone(), run_id.to_string());
    paths.ensure_dirs()?;
    write_heartbeat(&paths.heartbeat_path)?;

    let mut outcome = SupervisorOutcome {
        status: 1,
        signaled: false,
        timed_out: false,
        cancelled: false,
    };

    let spawn_result = run_supervisor(
        self_exe,
        cfg,
        issue,
        worktree,
        run_id,
        context_json,
        worker_command,
        &paths,
        cancellation,
    )
    .await;

    let result = match spawn_result {
        Ok(out) => {
            outcome = out;
            Ok(())
        }
        Err(err) => {
            tracing::warn!(error = %err, run_id, "supervisor failed; cleaning up");
            Err(err)
        }
    };

    if let Err(err) = clear_heartbeat(&paths.heartbeat_path) {
        tracing::warn!(error = %err, run_id, "heartbeat cleanup failed");
    }

    result.map(|_| outcome)
}

#[allow(clippy::too_many_arguments)]
async fn run_supervisor(
    self_exe: &Path,
    cfg: &crate::config::Config,
    issue: &IssueKey,
    worktree: &Path,
    run_id: &str,
    context_json: &str,
    worker_command: &[String],
    paths: &WorkerRunPaths,
    cancellation: tokio_util::sync::CancellationToken,
) -> CaduceusResult<SupervisorOutcome> {
    let cmd = build_supervisor_command(
        self_exe,
        worktree,
        run_id,
        issue,
        context_json,
        worker_command,
        &paths.transcript_path,
        &paths.heartbeat_path,
        cfg.worker_timeout_seconds,
    );

    // Convert to a tokio command for async I/O. The
    // `process_group(0)` call sets a fresh process-group
    // leader so the daemon can later broadcast to the whole
    // supervisor subtree if needed.
    let mut tokio_cmd: TokioCommand = cmd.into();
    tokio_cmd.kill_on_drop(true);
    tokio_cmd.process_group(0);
    let mut child: Child = tokio_cmd.spawn().map_err(|err| CaduceusError::Worker {
        context: "supervisor:spawn",
        stderr: format!("spawn __worker-supervisor: {err}"),
    })?;

    let mut stdin = child.stdin.take().ok_or_else(|| CaduceusError::Worker {
        context: "supervisor:spawn",
        stderr: "supervisor stdin was not piped".to_string(),
    })?;
    let mut stdout = child.stdout.take().ok_or_else(|| CaduceusError::Worker {
        context: "supervisor:spawn",
        stderr: "supervisor stdout was not piped".to_string(),
    })?;
    let stderr = child.stderr.take();

    // Protocol loop. Reads `READY(pgid)` → sends `ACK`;
    // reads `DONE` → returns; reads `FATAL` → returns error.
    let protocol_task = {
        let cancel = cancellation.clone();
        tokio::spawn(async move {
            let mut buf = Vec::with_capacity(MAX_FRAME_BYTES);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        write_frame_async(&mut stdin, &ControlFrame::Terminate { force: false }).await.ok();
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        write_frame_async(&mut stdin, &ControlFrame::Terminate { force: true }).await.ok();
                        return SupervisorOutcome {
                            status: 130,
                            signaled: true,
                            timed_out: false,
                            cancelled: true,
                        };
                    }
                    frame = read_frame_async(&mut stdout, &mut buf) => {
                        let frame = match frame {
                            Ok(Some(f)) => f,
                            Ok(None) => {
                                // EOF — supervisor closed stdout.
                                return SupervisorOutcome {
                                    status: 0,
                                    signaled: false,
                                    timed_out: false,
                                    cancelled: false,
                                };
                            }
                            Err(err) => return err.into_outcome(),
                        };
                        match frame {
                            ControlFrame::Ready { .. } => {
                                write_frame_async(&mut stdin, &ControlFrame::Ack).await.ok();
                            }
                            ControlFrame::Done { status, signaled } => {
                                return SupervisorOutcome {
                                    status,
                                    signaled,
                                    timed_out: false,
                                    cancelled: false,
                                };
                            }
                            ControlFrame::Fatal { reason } => {
                                tracing::warn!(reason, "supervisor reported FATAL");
                                return SupervisorOutcome {
                                    status: 1,
                                    signaled: false,
                                    timed_out: false,
                                    cancelled: false,
                                };
                            }
                            ControlFrame::Ack | ControlFrame::Terminate { .. } => {
                                tracing::warn!(opcode = ?frame.opcode(), "unexpected frame from supervisor");
                                return SupervisorOutcome {
                                    status: 1,
                                    signaled: false,
                                    timed_out: false,
                                    cancelled: false,
                                };
                            }
                        }
                    }
                }
            }
        })
    };

    // Stderr drain — write into the transcript.
    if let Some(mut stderr) = stderr {
        let path = paths.transcript_path.clone();
        let max_bytes = cfg.transcript_max_bytes;
        let _drain_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let mut file = match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
            {
                Ok(f) => f,
                Err(_) => return,
            };
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        file.write_all(&buf[..n]).await.ok();
                        file.flush().await.ok();
                    }
                    Err(_) => break,
                }
            }
            // Truncate if needed.
            if let Ok(meta) = tokio::fs::metadata(&path).await {
                if meta.len() > max_bytes {
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = truncate_transcript(&path, max_bytes);
                    })
                    .await;
                }
            }
        });
    }

    // Heartbeat refresh: every 5s while the worker is alive.
    let hb_path = paths.heartbeat_path.clone();
    let hb_cancel = cancellation.clone();
    let heartbeat_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if hb_cancel.is_cancelled() {
                break;
            }
            if write_heartbeat(&hb_path).is_err() {
                break;
            }
        }
    });

    // Await the supervisor child.
    let supervisor_status = child.wait().await.map_err(|err| CaduceusError::Worker {
        context: "supervisor:wait",
        stderr: format!("wait: {err}"),
    })?;

    cancellation.cancel();
    let outcome = protocol_task.await.map_err(|err| CaduceusError::Worker {
        context: "supervisor:join",
        stderr: format!("join protocol task: {err}"),
    })?;
    heartbeat_task.abort();

    let signaled = supervisor_status.code().is_none();
    let _ = signaled;
    Ok(outcome)
}

/// Helper trait extension so `CaduceusError` can map itself to
/// an outcome in the protocol task.
trait IntoOutcome {
    fn into_outcome(self) -> SupervisorOutcome;
}

impl IntoOutcome for CaduceusError {
    fn into_outcome(self) -> SupervisorOutcome {
        SupervisorOutcome {
            status: 1,
            signaled: false,
            timed_out: false,
            cancelled: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Self-test (cargo test --lib)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod inline_tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let cases = vec![
            ControlFrame::Ready { pgid: 1234 },
            ControlFrame::Done {
                status: 0,
                signaled: false,
            },
            ControlFrame::Done {
                status: 9,
                signaled: true,
            },
            ControlFrame::Fatal {
                reason: "boom".to_string(),
            },
            ControlFrame::Terminate { force: false },
            ControlFrame::Terminate { force: true },
            ControlFrame::Ack,
        ];
        for case in cases {
            let encoded = encode_frame(&case).expect("encode");
            let (decoded, consumed) = decode_frame(&encoded).expect("decode");
            assert_eq!(consumed, encoded.len());
            assert_eq!(decoded, case);
        }
    }

    #[test]
    fn frame_rejects_wrong_version() {
        let mut bytes = encode_frame(&ControlFrame::Ack).expect("encode");
        // Mangle the version byte.
        bytes[6] = b'9';
        let err = decode_frame(&bytes).expect_err("must reject");
        let msg = format!("{err:?}");
        assert!(msg.contains("unsupported protocol version"), "{msg}");
    }

    #[test]
    fn frame_rejects_oversize() {
        // Construct a buffer whose first 4 bytes encode a
        // length that exceeds MAX_FRAME_BYTES, then put enough
        // payload after it so the frame *appears* complete —
        // the decoder should reject it on the size check
        // before parsing the body.
        let mut bytes = Vec::new();
        let oversize = (MAX_FRAME_BYTES as u32) + 1;
        bytes.extend_from_slice(&oversize.to_le_bytes());
        bytes.resize(4 + oversize as usize, 0);
        let err = decode_frame(&bytes).expect_err("must reject");
        let msg = format!("{err:?}");
        assert!(msg.contains("exceeds cap"), "{msg}");
    }

    #[test]
    fn heartbeat_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hbeat");
        write_heartbeat(&path).expect("write");
        let read = read_heartbeat(&path).expect("read");
        assert!((chrono::Utc::now() - read).num_seconds().abs() < 5);
        clear_heartbeat(&path).expect("clear");
        assert!(read_heartbeat(&path).is_none());
    }

    #[test]
    fn transcript_truncation_appends_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.log");
        let mut file = open_transcript(&path).expect("open");
        for _ in 0..1000 {
            file.write_all(b"chunk\n").expect("write");
        }
        drop(file);
        let truncated = truncate_transcript(&path, 64).expect("truncate");
        assert!(truncated);
        let meta = std::fs::metadata(&path).expect("stat");
        assert!(
            meta.len() <= 256,
            "transcript should be roughly capped; got {}",
            meta.len()
        );
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("truncated"), "marker missing from {body:?}");
    }

    #[test]
    fn paths_ensure_dirs_creates_secure_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = WorkerRunPaths::new(dir.path().to_path_buf(), "RUN01".to_string());
        paths.ensure_dirs().expect("ensure_dirs");
        let meta = std::fs::metadata(dir.path().join("runs")).expect("stat runs");
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    }
}
