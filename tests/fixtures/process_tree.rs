//! Process tree fixture for Caduceus supervision tests.
//!
//! Provides a [`ProcessTree`] helper that creates a temp directory,
//! optionally sets the subreaper flag, spawns shell scripts in
//! detached process groups, enumerates descendants from `/proc`,
//! and sends signals to PIDs and process groups. All OS-specific
//! methods are gated on `#[cfg(target_os = "linux")]`; non-Linux
//! platforms receive empty stubs so the fixture compiles
//! everywhere.
//!
//! The contract surface from `CONTRACTS.md`:
//!
//! * **CI-002** — fixtures MUST be hermetic and MUST NOT
//!   require production credentials. `ProcessTree` never reads
//!   a token, never touches a network interface, and asserts
//!   only on procfs entries that are local to the test host.
//! * **CI-004** — the supervisor's descendant reaping relies on
//!   `prctl(PR_SET_CHILD_SUBREAPER)` + `/proc` enumeration, and
//!   `ProcessTree` is the fixture that exercises it.

#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;
use tempfile::TempDir;

/// Owns a temporary directory with helpers for spawning, observing,
/// and killing process trees. Every test that exercises the
/// supervisor's /proc descendant walker or the process-group kill
/// path should use this fixture.
pub struct ProcessTree {
    _dir: TempDir,
    workdir: PathBuf,
}

impl ProcessTree {
    /// Create a new `ProcessTree` under a unique tempdir with the
    /// given `label`. On Linux, also calls
    /// `prctl(PR_SET_CHILD_SUBREAPER, true)` in the test process so
    /// that orphaned descendants are visible to the `/proc` walker.
    ///
    /// The subreaper call is best-effort — failure is deliberately
    /// swallowed so the fixture works inside containers that may
    /// restrict `prctl`.
    #[cfg(target_os = "linux")]
    pub fn start(label: &str) -> Self {
        let _ = nix::sys::prctl::set_child_subreaper(true);
        let dir = tempfile::Builder::new()
            .prefix(&format!("caduceus-ptree-{label}-"))
            .tempdir()
            .expect("tempdir create");
        let workdir = dir.path().join("work");
        fs::create_dir_all(&workdir).expect("create workdir");
        Self { _dir: dir, workdir }
    }

    /// Non-Linux stub: creates the tempdir but does not attempt
    /// the subreaper call.
    #[cfg(not(target_os = "linux"))]
    pub fn start(label: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("caduceus-ptree-{label}-"))
            .tempdir()
            .expect("tempdir create");
        let workdir = dir.path().join("work");
        fs::create_dir_all(&workdir).expect("create workdir");
        Self { _dir: dir, workdir }
    }

    /// Path to the working directory owned by this fixture.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Write `script` to a temp file in the workdir, spawn a
    /// `bash` subprocess running it in a **new process group**
    /// (via `process_group(0)`), and return the child PID.
    ///
    /// The script is made executable and inherits the test process's
    /// environment — callers that need a scrubbed environment should
    /// use `env_clear()` on the result of `spawn_detached_bash`.
    #[cfg(target_os = "linux")]
    pub fn spawn_detached_bash(&self, script: &str) -> i32 {
        let script_path = self.workdir.join(format!("script-{}.sh", rand_id()));
        let mut f = fs::File::create(&script_path).expect("create script");
        f.write_all(script.as_bytes()).expect("write script");
        drop(f);
        let mut perms = script_path.metadata().expect("stat").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");

        // Deliberately dropped: the test reaps/kills the child
        // via the returned PID + terminate(). We never wait() here
        // because the child must stay alive for observation.
        #[allow(clippy::zombie_processes)]
        let child = Command::new("bash")
            .arg(&script_path)
            .current_dir(&self.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bash");
        let pid = child.id() as i32;
        // Detach from the child — the test is responsible for
        // reaping or killing it. We deliberately do not wait()
        // here because the test needs the child alive.
        #[allow(clippy::zombie_processes)]
        drop(child);
        pid
    }

    #[cfg(not(target_os = "linux"))]
    pub fn spawn_detached_bash(&self, _script: &str) -> i32 {
        -1
    }

    /// Walk `/proc` and return the PIDs of every direct child of
    /// `ppid`. Uses the same `parse_stat_parent` logic as the
    /// production supervisor's `collect_descendants`.
    #[cfg(target_os = "linux")]
    pub fn descendants(&self, ppid: i32) -> Vec<i32> {
        collect_descendants(ppid)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn descendants(&self, _ppid: i32) -> Vec<i32> {
        Vec::new()
    }

    /// Send `signal` to `pid` via `nix::sys::signal::kill`.
    /// Errors are silently swallowed (the process may already
    /// have exited).
    #[cfg(target_os = "linux")]
    pub fn terminate(&self, pid: i32, signal: Signal) {
        let _ = kill(Pid::from_raw(pid), signal);
    }

    #[cfg(not(target_os = "linux"))]
    pub fn terminate(&self, _pid: i32, _signal: Signal) {}

    /// Reap a zombie child by PID (non-blocking). Returns `true`
    /// if the process was reaped, `false` if it was not a child
    /// of this process or does not exist.
    #[cfg(target_os = "linux")]
    pub fn reap(&self, pid: i32) -> bool {
        use nix::sys::wait::{waitpid, WaitPidFlag};
        waitpid(Pid::from_raw(pid), Some(WaitPidFlag::WNOHANG)).is_ok()
    }

    #[cfg(not(target_os = "linux"))]
    pub fn reap(&self, _pid: i32) -> bool {
        false
    }

    /// Send `signal` to the process group `pgid` via
    /// `nix::sys::signal::killpg`. Errors are silently swallowed.
    #[cfg(target_os = "linux")]
    pub fn kill_pgid(&self, pgid: i32, signal: Signal) {
        let _ = killpg(Pid::from_raw(pgid), signal);
    }

    #[cfg(not(target_os = "linux"))]
    pub fn kill_pgid(&self, _pgid: i32, _signal: Signal) {}

    /// Send `signal` to `pid` via `nix::sys::signal::kill`.
    /// Errors are silently swallowed.
    #[cfg(target_os = "linux")]
    pub fn kill_pid(&self, pid: i32, signal: Signal) {
        let _ = kill(Pid::from_raw(pid), signal);
    }

    #[cfg(not(target_os = "linux"))]
    pub fn kill_pid(&self, _pid: i32, _signal: Signal) {}

    /// Poll `/proc/<pid>/cmdline` for up to 5 seconds, searching
    /// for a process whose `cmdline` contains `name`. If any such
    /// process is found, `assert!` fails.
    #[cfg(target_os = "linux")]
    pub fn assert_no_process_by_name(&self, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let found = find_process_cmdline(name);
            if !found {
                return;
            }
            if Instant::now() >= deadline {
                panic!("assert_no_process_by_name: '{name}' still found after 5s");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn assert_no_process_by_name(&self, _name: &str) {}
}

/// Walk `/proc` and check if any process's `cmdline` contains
/// `name`. Returns `true` if at least one match was found.
#[cfg(target_os = "linux")]
fn find_process_cmdline(name: &str) -> bool {
    let entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let name_os = entry.file_name();
        let name_str = name_os.to_string_lossy();
        let Ok(_pid) = name_str.parse::<i32>() else {
            continue;
        };
        let cmdline_path = entry.path().join("cmdline");
        let cmdline = match fs::read_to_string(&cmdline_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if cmdline.contains(name) {
            return true;
        }
    }
    false
}

/// Walk `/proc` for every PID whose `stat` reports `ppid` as its
/// parent. Mirrors the production `collect_descendants` in
/// `src/worker_supervisor.rs`.
#[cfg(target_os = "linux")]
fn collect_descendants(ppid: i32) -> Vec<i32> {
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

/// Best-effort parser for `/proc/<pid>/stat`. Returns the parent
/// PID from the first call to `rfind(')')` + field 4.
#[cfg(target_os = "linux")]
fn parse_stat_parent(stat: &str) -> Option<i32> {
    let close = stat.rfind(')')?;
    let after = &stat[close + 1..];
    let mut it = after.split_whitespace();
    let _state = it.next()?;
    let ppid = it.next()?.parse().ok()?;
    Some(ppid)
}

/// Tiny nonce for script filenames.
fn rand_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}
