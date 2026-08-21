use std::path::Path;
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
use libc::{c_int, c_uint, c_void};

use crate::github::issue::IssueKey;
use crate::infra::error::{CaduceusError, CaduceusResult};

// Hidden command name

/// Hidden command name that the binary recognises before public
/// Clap parsing, matched only as the first argument after the
/// binary name (`argv[1]`). The token is reserved and must never
/// appear in `--help` output or be accepted from cron / plugin
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

/// Process identity operations used to distinguish a live process from a
/// recycled PID.
pub trait ProcessIdentity: Sync {
    fn start_ticks(&self, pid: i32) -> Option<u64>;
    fn is_alive(&self, pid: i32) -> bool;
    fn verify(&self, pid: i32, expected_start_ticks: u64) -> bool;
}

/// Process-tree operations used by the supervisor and reaper.
pub trait ProcessTree: Sync {
    fn adopt_subtree(&self, root: i32) -> std::io::Result<()>;
    fn list_children(&self, ppid: i32) -> Vec<i32>;
}

#[cfg(target_os = "macos")]
const PROC_PIDTBSDINFO: c_int = 3;

#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, dead_code)]
#[repr(C)]
pub(crate) struct proc_bsdinfo {
    pub(crate) pbi_flags: c_uint,
    pub(crate) pbi_status: c_uint,
    pub(crate) pbi_xstatus: c_uint,
    pub(crate) pbi_pid: c_uint,
    pub(crate) pbi_ppid: c_uint,
    pub(crate) pbi_uid: c_uint,
    pub(crate) pbi_gid: c_uint,
    pub(crate) pbi_ruid: c_uint,
    pub(crate) pbi_rgid: c_uint,
    pub(crate) pbi_svuid: c_uint,
    pub(crate) pbi_svgid: c_uint,
    pub(crate) rfu_1: c_uint,
    pub(crate) pbi_comm: [c_uint; 4],
    pub(crate) pbi_name: [c_uint; 8],
    pub(crate) pbi_nfiles: c_uint,
    pub(crate) pbi_pgid: c_uint,
    pub(crate) pbi_pjobc: c_uint,
    pub(crate) e_tdev: c_uint,
    pub(crate) e_tpgid: c_uint,
    pub(crate) pbi_nice: c_int,
    pub(crate) pbi_start_tv_sec: u64,
    pub(crate) pbi_start_tv_usec: u64,
}

#[cfg(target_os = "macos")]
const _: () = assert!(std::mem::size_of::<proc_bsdinfo>() == 136);

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct MacOsProcessIdentity;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
impl MacOsProcessIdentity {
    fn read_proc_bsdinfo(&self, pid: i32) -> Option<proc_bsdinfo> {
        extern "C" {
            fn proc_pidinfo(
                pid: c_int,
                flavor: c_int,
                arg: u64,
                buffer: *mut c_void,
                buffersize: c_int,
            ) -> c_int;
        }

        let expected_size = std::mem::size_of::<proc_bsdinfo>() as c_int;
        let mut info = std::mem::MaybeUninit::<proc_bsdinfo>::uninit();
        let returned = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                expected_size,
            )
        };
        if returned != expected_size {
            tracing::debug!(
                pid,
                returned,
                expected = expected_size,
                "proc_pidinfo(PROC_PIDTBSDINFO) returned an unexpected size"
            );
            return None;
        }
        if pid <= 0 {
            return None;
        }

        Some(unsafe { info.assume_init() })
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
impl ProcessIdentity for MacOsProcessIdentity {
    /// `proc_pidinfo` succeeds for zombies, so the reaper treats them as live
    /// because the supervisor has not yet reaped them. PID recycling is
    /// disambiguated by the `start_ticks` check rather than by treating
    /// zombies as dead. The `pbi_status` field is intentionally not inspected;
    /// the supervisor owns the wait and the reaper's zombie stance is to leave
    /// the claim alone until that wait completes.
    fn is_alive(&self, pid: i32) -> bool {
        self.read_proc_bsdinfo(pid).is_some()
    }

    fn start_ticks(&self, pid: i32) -> Option<u64> {
        // These are microseconds since the Unix epoch. Units are never
        // cross-compared: process_start_identity pairs each platform's ticks
        // with its own boot epoch, and identity strings are only compared for
        // equality on the same machine. `proc_bsdinfo` exposes no finer
        // resolution, so the microsecond collision question remains a
        // reviewer-visible trade-off rather than changing this identity unit.
        self.read_proc_bsdinfo(pid)
            .map(|info| info.pbi_start_tv_sec as u64 * 1_000_000 + info.pbi_start_tv_usec as u64)
    }

    fn verify(&self, pid: i32, expected_start_ticks: u64) -> bool {
        self.is_alive(pid) && self.start_ticks(pid) == Some(expected_start_ticks)
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct LinuxProcessIdentity;

#[cfg(target_os = "linux")]
impl ProcessIdentity for LinuxProcessIdentity {
    fn start_ticks(&self, pid: i32) -> Option<u64> {
        read_proc_starttime_impl(pid)
    }

    fn is_alive(&self, pid: i32) -> bool {
        pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
    }

    fn verify(&self, pid: i32, expected_start_ticks: u64) -> bool {
        read_proc_starttime_impl(pid) == Some(expected_start_ticks)
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct LinuxProcessTree;

#[cfg(target_os = "linux")]
impl ProcessTree for LinuxProcessTree {
    fn adopt_subtree(&self, _root: i32) -> std::io::Result<()> {
        nix::sys::prctl::set_child_subreaper(true)
            .map_err(|err| std::io::Error::other(err.to_string()))
    }

    fn list_children(&self, ppid: i32) -> Vec<i32> {
        collect_descendants(ppid)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[derive(Debug)]
pub struct StubProcessIdentity;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl ProcessIdentity for StubProcessIdentity {
    fn start_ticks(&self, _pid: i32) -> Option<u64> {
        None
    }

    fn is_alive(&self, _pid: i32) -> bool {
        false
    }

    fn verify(&self, _pid: i32, _expected_start_ticks: u64) -> bool {
        false
    }
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct StubProcessTree;

#[cfg(not(target_os = "linux"))]
impl ProcessTree for StubProcessTree {
    fn adopt_subtree(&self, _root: i32) -> std::io::Result<()> {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| tracing::warn!("child subreaper is unavailable on this platform"));
        Ok(())
    }

    fn list_children(&self, _ppid: i32) -> Vec<i32> {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
pub static IDENTITY: &dyn ProcessIdentity = &LinuxProcessIdentity;

#[cfg(target_os = "macos")]
pub static IDENTITY: &dyn ProcessIdentity = &MacOsProcessIdentity;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub static IDENTITY: &dyn ProcessIdentity = &StubProcessIdentity;

#[cfg(target_os = "linux")]
pub static TREE: &dyn ProcessTree = &LinuxProcessTree;

#[cfg(not(target_os = "linux"))]
pub static TREE: &dyn ProcessTree = &StubProcessTree;

// Subreaper + setsid + signal helpers (safe wrappers via nix)

/// `setsid` the calling process into a new session.
///
/// Called in the register phase, before the worker is forked and
/// before the `READY{pgid}` frame is sent; the daemon records the
/// returned PGID (== the supervisor's PID) and only then `ACK`s, at
/// which point the supervisor spawns the worker.
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

// Process-identity helpers — read /proc/<pid>/stat starttime to detect PID
// reuse before signalling.

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

/// Test seam: re-export the synthetic-stat parser so integration
/// tests can assert the field-22 contract without owning a runtime.
/// Identical to the private [`parse_starttime_from_stat`].
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn parse_starttime_from_stat_for_tests(stat: &str) -> Option<u64> {
    parse_starttime_from_stat(stat)
}

#[cfg(target_os = "linux")]
fn read_proc_starttime_impl(pid: i32) -> Option<u64> {
    let body = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_starttime_from_stat(&body)
}

/// Read process starttime in clock ticks from `/proc/<pid>/stat`,
/// field 22.  Returns `None` if the process no longer exists or the
/// stat file cannot be read.
#[cfg(target_os = "linux")]
pub fn read_proc_starttime(pid: i32) -> Option<u64> {
    IDENTITY.start_ticks(pid)
}

/// Return `true` only when *pid* still refers to the same process
/// incarnation whose starttime was *expected_starttime*.  Returns
/// `false` if the process has exited (PID recycled) or the starttime
/// differs (PID reuse).
#[cfg(target_os = "linux")]
pub fn verify_identity(pid: i32, expected_starttime: u64) -> bool {
    IDENTITY.verify(pid, expected_starttime)
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

/// Translate the signal names used by the supervisor control protocol to
/// their portable Unix signal numbers.
#[cfg(unix)]
pub fn signal_number_from_str(s: &str) -> Option<i32> {
    match s {
        "TERM" | "SIGTERM" => Some(15),
        "KILL" | "SIGKILL" => Some(9),
        _ => None,
    }
}

// Hidden command dispatch + env construction

/// Build the `caduceus __worker-supervisor` command for *args*.
/// The hidden command is dispatched before Clap parsing so it
/// is never shown in `--help` output and is never accepted
/// from cron / plugin configuration. The supervisor inherits
/// the daemon environment so PATH and other safe bootstrap
/// variables are available; the worker subprocess is then
/// launched with `env_clear()` and a sanitized environment
/// built by `run_supervisor_mode`.
///
/// The daemon-side uses `Child::stdin/stdout/stderr` for the
/// control/status pipes — the supervisor inherits them as
/// the canonical "inherited file descriptors" the contract
/// requires. The supervisor defers spawning the worker until
/// the daemon acknowledges the `READY(pgid)` frame with
/// `ACK`; until then no child process exists.
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
    issue_title: &str,
    issue_body: &str,
    labels: &[String],
    branch_name: &str,
) -> Command {
    let labels_json = serde_json::to_string(labels).unwrap_or_else(|_| "[]".to_string());
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
    cmd.arg("--issue-title").arg(issue_title);
    cmd.arg("--issue-body").arg(issue_body);
    cmd.arg("--issue-labels-json").arg(&labels_json);
    cmd.arg("--branch-name").arg(branch_name);
    cmd.arg("--");
    for arg in worker_command {
        cmd.arg(arg);
    }
    // The supervisor's stdin/stdout are the daemon's control/status
    // pipes. The supervisor owns the transcript file (worker
    // stdout+stderr); its own diagnostics inherit to the daemon's
    // stderr so they reach the operator/journald without contending
    // for the transcript.
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());
    cmd
}
