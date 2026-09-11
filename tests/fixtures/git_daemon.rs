//! Disposable `git daemon` origin fixture.
//!
//! Serves a bare repo over the `git://` protocol on 127.0.0.1 so
//! the daemon's `git fetch` / `git push` exercise real git
//! subprocesses without github.com. The host `127.0.0.1` matches
//! the wiremock `api_base` host, so `validate_origin_host` accepts
//! the origin (this is why a `git daemon` is needed rather than a
//! `file://` `LocalOrigin`).
//!
//! Shared by `tests/daemon/per_claim_test.rs` and
//! `tests/integration/release_canary_test.rs`. Each consumer wires
//! the fixture in via `#[path = "fixtures/mod.rs"] mod fixtures;`
//! and imports what it uses. The `#![allow(dead_code)]` covers
//! methods only one consumer binary exercises — same rationale as
//! `github.rs` and `git_origin.rs`.
//!
//! Ownership protocol (issue #383): a hard parent death — SIGKILL,
//! `panic=abort`, or a crash — skips every destructor, so the daemon
//! would be reparented and keep listening forever and the fixture
//! root would persist. `start()` therefore (1) first reaps leftovers
//! from dead runs (`reap_stale_git_daemons`), (2) acquires an
//! exclusive `flock` on `<root>/git-daemon.lock` for the daemon's
//! lifetime — the kernel releases it automatically when this process
//! dies — and (3) writes the child pid to `<root>/git-daemon.pid`
//! right after spawn. The lock is acquired BEFORE the pidfile
//! exists, so a pidfile present in the temp dir always means "a live
//! owner holds the lock, or that owner is dead"; the reaper only
//! ever acts on roots whose lock is provably free.

#![allow(dead_code)]

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tempfile::TempDir;

/// Owns a `git daemon` subprocess serving a bare repo at
/// `git://127.0.0.1:<port>/<owner>/<repo>`. The child is killed on
/// drop before the tempdir is removed; stale daemons orphaned by
/// interrupted runs are reaped on the next `start`.
pub struct GitDaemon {
    _root: TempDir,
    bare: PathBuf,
    port: u16,
    child: std::process::Child,
    owner: String,
    repo: String,
    pidfile: PathBuf,
    // Declared LAST so it drops after `_root` (struct fields drop in
    // declaration order): unlocking a lock file that was just unlinked
    // with the tempdir is harmless.
    _lock: Flock<std::fs::File>,
}

impl GitDaemon {
    /// Create the bare repo, seed an empty commit on `main`, and start
    /// `git daemon` on a free 127.0.0.1 port. `owner`/`repo` form the
    /// `git://` path the daemon's clone should use.
    ///
    /// If the daemon fails to become ready, the spawned child is killed
    /// before this function panics, so no orphan `git daemon` leaks.
    pub fn start(label: &str, owner: &str, repo: &str) -> Self {
        // Self-heal: first reap daemons left behind by previous runs
        // that died hard (SIGKILL, abort, crash) and skipped Drop.
        reap_stale_git_daemons();

        let root = TempDir::with_prefix(format!("caduceus-origin-{label}-")).expect("origin");
        let gitroot = root.path().join("gitroot");
        let bare = gitroot.join(owner).join(repo);
        fs::create_dir_all(&bare).expect("mkdir bare");
        init_bare_with_empty_main(&bare);

        // Enable push over the git protocol.
        git_in(&bare, &["config", "daemon.receivepack", "true"]);
        git_in(&bare, &["config", "daemon.uploadarch", "true"]);

        // Owner guard: the kernel releases this flock automatically if
        // this process dies hard, so a later run can distinguish "live
        // owner" from "dead run" without heuristics. Acquired BEFORE
        // the pidfile exists — that ordering is what makes a pidfile
        // in the temp dir imply "lock held OR owner dead".
        let lock_path = root.path().join("git-daemon.lock");
        fs::File::create(&lock_path).expect("create daemon lock");
        let lock_file = fs::File::open(&lock_path).expect("open daemon lock");
        let lock = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock)
            .unwrap_or_else(|(_, errno)| panic!("git-daemon.lock acquire: {errno}"));

        let port = free_port_127();
        let log_path = root.path().join("git-daemon.log");
        let log = fs::File::create(&log_path).expect("create daemon log");
        // Guard the child so a readiness panic kills it instead of
        // orphaning a `git daemon` that holds no TempDir owner yet.
        let mut child = {
            struct KillOnDrop(Option<std::process::Child>);
            impl Drop for KillOnDrop {
                fn drop(&mut self) {
                    if let Some(mut c) = self.0.take() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                }
            }
            KillOnDrop(Some(
                Command::new("git")
                    .args([
                        "daemon",
                        "--reuseaddr",
                        "--listen=127.0.0.1",
                        &format!("--port={port}"),
                        &format!("--base-path={}", gitroot.display()),
                        "--export-all",
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(log.try_clone().expect("clone log")))
                    .stderr(Stdio::from(log))
                    .spawn()
                    .unwrap_or_else(|e| panic!("spawn git daemon: {e}")),
            ))
        };

        // Record the child pid BEFORE the readiness wait so a hard
        // death inside the readiness window still leaves a pidfile for
        // the next run's reaper. A half-ready daemon in the pidfile is
        // harmless: the reap only acts on roots whose lock is free.
        let pidfile = root.path().join("git-daemon.pid");
        let pid = child.0.as_ref().expect("child present after spawn").id();
        fs::write(&pidfile, format!("{pid}\n")).expect("write daemon pidfile");

        // Wait for the daemon to accept a connection so the clone below
        // does not race the bind. Also confirm the child has not already
        // exited (a bind error would make it die before readiness).
        wait_for_port_127(port, Duration::from_secs(5));
        match child.0.as_mut().unwrap().try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                let log = fs::read_to_string(&log_path).unwrap_or_default();
                panic!("git daemon exited before becoming ready (status {status}); log:\n{log}");
            }
            Err(e) => panic!("git daemon readiness try_wait: {e}"),
        }

        let child = child.0.take().expect("child present after readiness");
        Self {
            _root: root,
            bare,
            port,
            child,
            owner: owner.to_string(),
            repo: repo.to_string(),
            pidfile,
            _lock: lock,
        }
    }

    /// `git://127.0.0.1:<port>/<owner>/<repo>` — the URL the daemon's
    /// clone should use as `remote.origin.url`.
    pub fn uri(&self) -> String {
        format!("git://127.0.0.1:{}/{}/{}", self.port, self.owner, self.repo)
    }

    /// Path to the bare repository directory on disk.
    pub fn path(&self) -> &Path {
        &self.bare
    }

    /// Path to the fixture root directory, which holds
    /// `git-daemon.log`, `git-daemon.lock`, and `git-daemon.pid`.
    pub fn root(&self) -> &Path {
        self._root.path()
    }

    /// Number of refs under `refs/heads/` in the bare repo. Used to
    /// prove a scheduled tick pushed exactly one new branch.
    pub fn head_refs(&self) -> Vec<String> {
        let out = Command::new("git")
            .current_dir(&self.bare)
            .args(["for-each-ref", "--format=%(refname)", "refs/heads/"])
            .output()
            .expect("for-each-ref");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect()
    }

    /// Count commits on `refs/heads/main`.
    pub fn main_commit_count(&self) -> usize {
        rev_list_count(&self.bare, "refs/heads/main")
    }

    /// Count commits on `branch` that are not on `main` (the new work
    /// a scheduled tick pushed).
    pub fn branch_commits_beyond_main(&self, branch: &str) -> usize {
        let out = Command::new("git")
            .current_dir(&self.bare)
            .args(["rev-list", "--count", branch, "^refs/heads/main"])
            .output()
            .expect("rev-list count");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<usize>()
            .unwrap_or(0)
    }
}

impl Drop for GitDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Best-effort: the tempdir removal that follows takes the file
        // with it if this removal fails.
        let _ = fs::remove_file(&self.pidfile);
    }
}

/// Outcome of a stale-daemon sweep: how many daemons were killed and
/// how many fixture roots were removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReapSummary {
    pub daemons_killed: usize,
    pub roots_removed: usize,
}

/// Kill `git daemon` processes left behind by interrupted test runs
/// and remove their fixture roots.
///
/// Scans `std::env::temp_dir()` for `caduceus-origin-*` roots. A root
/// is only touched when ALL of these hold: it has a `git-daemon.pid`,
/// its `git-daemon.lock` is FREE (kernel-verified: no live owner), the
/// recorded pid is alive, and that process's argv identifies it as a
/// `git daemon` serving exactly this root's `gitroot` base path. Any
/// unprovable step fails safe (skip, never signal). Never panics and
/// never blocks: every fallible step is best-effort and the lock probe
/// is non-blocking, so a reap failure cannot break a test run.
pub fn reap_stale_git_daemons() -> ReapSummary {
    let mut summary = ReapSummary::default();
    let temp_dir = std::env::temp_dir();
    let Ok(entries) = fs::read_dir(&temp_dir) else {
        return summary;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with("caduceus-origin-") {
            continue;
        }
        let root = entry.path();
        let pidfile = root.join("git-daemon.pid");
        if !pidfile.exists() {
            // LocalOrigin root, pre-fix legacy root, or a live root
            // mid-setup: never act without a pidfile.
            continue;
        }
        // Open the lock file read-only and probe it non-blocking.
        // EWOULDBLOCK means a LIVE owner: never touch that root.
        let Ok(lock_file) = fs::File::open(root.join("git-daemon.lock")) else {
            continue;
        };
        let Ok(_guard) = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) else {
            continue;
        };
        // The lock is free: the owning run is dead. `_guard` is held
        // for the rest of this iteration so a concurrent reaper cannot
        // double-act; it unlocks when it drops at the iteration end.
        let Ok(pid_text) = fs::read_to_string(&pidfile) else {
            continue;
        };
        let Ok(pid_num) = pid_text.trim().parse::<u32>() else {
            let _ = fs::remove_file(&pidfile);
            continue;
        };
        let pid = Pid::from_raw(pid_num as i32);
        match kill(pid, None) {
            Err(Errno::ESRCH) => {
                // Ownerless root whose pid is already gone: garbage.
                let _ = fs::remove_file(&pidfile);
                let _ = fs::remove_dir_all(&root);
                summary.roots_removed += 1;
                continue;
            }
            Ok(_) | Err(Errno::EPERM) => {}
            Err(_) => continue,
        }
        let expected_gitroot = root.join("gitroot");
        if !daemon_serves_base_path(pid, &expected_gitroot) {
            // Identity unprovable or wrong: NEVER kill. Drop the stale
            // pidfile, leave the root for forensics.
            let _ = fs::remove_file(&pidfile);
            continue;
        }
        // Re-validate immediately before the kill to shrink the
        // pid-recycle TOCTOU window to microseconds.
        if !daemon_serves_base_path(pid, &expected_gitroot) {
            let _ = fs::remove_file(&pidfile);
            continue;
        }
        let _ = kill(pid, Signal::SIGKILL); // ignore a raced death (ESRCH)
        summary.daemons_killed += 1;
        let _ = fs::remove_file(&pidfile);
        let _ = fs::remove_dir_all(&root);
        summary.roots_removed += 1;
        // `_guard` drops here, after the root is gone (unlocking an
        // unlinked file is harmless).
    }
    summary
}

/// True when process `pid`'s argv identifies it as a `git daemon`
/// serving exactly `expected_gitroot` as its base path.
///
/// Linux: read `/proc/<pid>/cmdline`. macOS: `ps -ww -p <pid> -o
/// command=`. Any read/spawn/parse failure is `false` (fail-safe:
/// unverifiable is never a kill reason).
#[cfg(target_os = "linux")]
fn daemon_serves_base_path(pid: Pid, expected_gitroot: &Path) -> bool {
    let Ok(cmdline) = fs::read_to_string(format!("/proc/{}/cmdline", pid.as_raw())) else {
        return false;
    };
    let argv: Vec<&str> = cmdline.split('\0').filter(|s| !s.is_empty()).collect();
    let Some(argv0) = argv.first() else {
        return false; // zombie with empty argv: identity unprovable
    };
    let basename = argv0.rsplit('/').next().unwrap_or(argv0);
    // `git daemon` is the git multi-call binary (argv[0]="git",
    // argv[1]="daemon"); some setups exec the dashed `git-daemon`
    // form. Accept both — the base-path match is the real guard.
    let is_git_daemon = basename.ends_with("git-daemon")
        || (basename == "git" && argv.get(1).is_some_and(|a| *a == "daemon"));
    if !is_git_daemon {
        return false;
    }
    let base_path_arg = format!("--base-path={}", expected_gitroot.display());
    argv.iter().any(|arg| *arg == base_path_arg)
}

#[cfg(target_os = "macos")]
fn daemon_serves_base_path(pid: Pid, expected_gitroot: &Path) -> bool {
    let Ok(out) = Command::new("ps")
        .args(["-ww", "-p", &pid.as_raw().to_string(), "-o", "command="])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let cmdline = String::from_utf8_lossy(&out.stdout);
    (cmdline.contains("git-daemon") || cmdline.contains("git daemon"))
        && cmdline.contains(&format!("--base-path={}", expected_gitroot.display()))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn daemon_serves_base_path(_pid: Pid, _expected_gitroot: &Path) -> bool {
    false
}

fn rev_list_count(bare: &Path, refspec: &str) -> usize {
    let out = Command::new("git")
        .current_dir(bare)
        .args(["rev-list", "--count", refspec])
        .output()
        .expect("rev-list --count");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<usize>()
        .unwrap_or(0)
}

/// `git init --bare` at `path`, then seed an empty commit on `main`.
pub fn init_bare_with_empty_main(path: &Path) {
    git_in(path, &["init", "--bare"]);
    git_in(path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let tree = String::from_utf8(
        Command::new("git")
            .current_dir(path)
            .args(["hash-object", "-w", "-t", "tree", "/dev/null"])
            .output()
            .expect("hash-object")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_string();
    let commit = String::from_utf8(
        Command::new("git")
            .current_dir(path)
            .args(["commit-tree", &tree, "-m", "initial"])
            .output()
            .expect("commit-tree")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_string();
    git_in(path, &["update-ref", "refs/heads/main", &commit]);
}

pub fn git_in(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("git spawn");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "git {:?} in {} failed ({}); stderr:\n{}",
            args,
            dir.display(),
            output.status,
            stderr
        );
    }
}

/// Grab a free TCP port on 127.0.0.1 by briefly binding, then drop the
/// listener so `git daemon` can take it.
pub fn free_port_127() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Poll until a TCP connection to `127.0.0.1:port` succeeds (the daemon
/// is accepting). Panics after `timeout`.
pub fn wait_for_port_127(port: u16, timeout: Duration) {
    let deadline = SystemTime::now() + timeout;
    while SystemTime::now() < deadline {
        if let Ok(addr) = format!("127.0.0.1:{port}").parse() {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("git daemon did not accept on 127.0.0.1:{port} within {timeout:?}");
}

/// Clone the bare origin into `workdir_base/<owner>/<repo>` so the
/// daemon's `find_main_clone` discovers it with `remote.origin.url` =
/// the origin's `git://` URI.
pub fn clone_main(workdir_base: &Path, origin_uri: &str, owner: &str, repo: &str) -> PathBuf {
    let main_path = workdir_base.join(owner).join(repo);
    fs::create_dir_all(workdir_base).expect("mkdir workdir_base");
    let output = Command::new("git")
        .args([
            "clone",
            "-b",
            "main",
            origin_uri,
            &main_path.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("git clone");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "git clone of disposable origin failed ({}); stderr:\n{}",
            output.status, stderr
        );
    }
    main_path
}

/// Spawn `cmd` and wait up to `timeout`, returning `(exit_code, stdout,
/// stderr)`. Uses `SystemTime` for the deadline. Panics if the process
/// does not exit in time (after killing it).
pub fn run_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
    label: &str,
) -> (i32, String, String) {
    use std::io::Read;
    let mut child = cmd.spawn().unwrap_or_else(|e| panic!("spawn {label}: {e}"));
    let deadline = SystemTime::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if SystemTime::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{label} did not exit within {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("{label} try_wait: {e}"),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut stdout);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut stderr);
    }
    (status.code().unwrap_or(-1), stdout, stderr)
}
