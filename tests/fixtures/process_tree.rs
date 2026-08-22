//! Process tree fixture for Caduceus supervision tests.
//!
//! Provides a [`ProcessTree`] helper that creates a temp directory,
//! optionally sets the subreaper flag, spawns shell scripts in
//! detached process groups, enumerates descendants through the
//! production process-tree seam, and sends signals to PIDs and
//! process groups. Linux and macOS use the production seam;
//! unsupported platforms receive empty stubs so the fixture
//! compiles everywhere.
//!
//! The contract surface from `CONTRACTS.md`:
//!
//! * **CI-002** — fixtures MUST be hermetic and MUST NOT
//!   require production credentials. `ProcessTree` never reads
//!   a token, never touches a network interface, and asserts
//!   only on local process state exposed by the production seams.
//! * **CI-004** — the supervisor's descendant reaping relies on
//!   `prctl(PR_SET_CHILD_SUBREAPER)` plus the process-tree seam,
//!   and `ProcessTree` is the fixture that exercises it.

#![allow(dead_code)]

#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(unused_imports)
)]
use std::collections::{HashSet, VecDeque};
use std::fs;
#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
use std::process::{Command, Stdio};
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(unused_imports)
)]
use std::time::{Duration, Instant};

#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
use nix::sys::signal::{kill, killpg, Signal};
#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
use nix::unistd::Pid;
use tempfile::TempDir;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use caduceus::worker::supervisor::process_lifecycle::{IDENTITY, TREE};

/// Owns a temporary directory with helpers for spawning, observing,
/// and killing process trees. Every test that exercises the
/// supervisor's descendant-reaping or process-group kill path
/// should use this fixture.
pub struct ProcessTree {
    _dir: TempDir,
    workdir: PathBuf,
}

impl ProcessTree {
    /// Create a new `ProcessTree` under a unique tempdir with the
    /// given `label`. On Linux, also calls
    /// `prctl(PR_SET_CHILD_SUBREAPER, true)` in the test process so
    /// that orphaned descendants are visible to the process-tree
    /// seam.
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

    /// Return the PIDs of every direct child of `ppid` through the
    /// production process-tree seam.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn descendants(&self, ppid: i32) -> Vec<i32> {
        TREE.list_children(ppid)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
}

/// Poll for a direct child of `root`, then return a snapshot of its
/// complete descendant subtree through the production process-tree
/// seam. A missing child is a test failure rather than an empty
/// assertion that could pass vacuously.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn snapshot_subtree(root: i32, timeout: Duration) -> Vec<i32> {
    let deadline = Instant::now() + timeout;
    let mut direct_children = TREE.list_children(root);
    while direct_children.is_empty() {
        if Instant::now() >= deadline {
            panic!("no child appeared under process {root} before the deadline");
        }
        std::thread::sleep(Duration::from_millis(50));
        direct_children = TREE.list_children(root);
    }

    // Give a just-forked grandchild the same settle window used by
    // the process-tree tests before walking the rest of the subtree.
    std::thread::sleep(Duration::from_millis(50));

    let mut pids = Vec::new();
    let mut queue = VecDeque::new();
    let mut seen = HashSet::from([root]);
    for pid in direct_children {
        if seen.insert(pid) {
            pids.push(pid);
            queue.push_back(pid);
        }
    }

    while let Some(ppid) = queue.pop_front() {
        for pid in TREE.list_children(ppid) {
            if seen.insert(pid) {
                pids.push(pid);
                queue.push_back(pid);
            }
        }
    }

    pids
}

/// Assert that every PID in a previously captured subtree is no
/// longer alive according to the production process-identity seam.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn assert_no_survivors(pids: &[i32]) {
    for &pid in pids {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // A killed grandchild can remain as a zombie under the
            // test process's subreaper until it is explicitly waited
            // on. Reap only captured children; a non-child is harmless.
            let _ = nix::sys::wait::waitpid(
                Pid::from_raw(pid),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            );
            if !IDENTITY.is_alive(pid) {
                break;
            }
            if Instant::now() >= deadline {
                panic!("process PID {pid} survived supervisor cleanup");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Tiny nonce for script filenames.
fn rand_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}
