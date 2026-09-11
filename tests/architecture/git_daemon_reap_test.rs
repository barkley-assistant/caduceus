//! Regression tests for the `GitDaemon` fixture's stale-daemon reaper
//! (issue #383).
//!
//! A hard parent death — SIGKILL, `panic=abort`, or a crash — skips
//! every destructor, so a `git daemon` spawned by the fixture is
//! reparented and keeps listening forever and its
//! `/tmp/caduceus-origin-*` root persists. `GitDaemon::start` now
//! writes `<root>/git-daemon.pid`, holds an exclusive `flock` on
//! `<root>/git-daemon.lock` for the daemon's lifetime, and first
//! reaps provably-ownerless leftovers (`reap_stale_git_daemons`).
//! These tests prove that reap decision tree end to end: the
//! interrupted-run shape is reaped, unrelated processes are never
//! signalled, live owners are untouched, and the pidfile lifecycle
//! holds.
//!
//! All tests are serial: they stage fake stale roots in the shared
//! temp dir, and while the flock guard (I4/I5) makes concurrent
//! reapers safe, the tests must not race each other.

#[path = "../fixtures/mod.rs"]
mod fixtures;

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use tempfile::TempDir;

use fixtures::{free_port_127, reap_stale_git_daemons, wait_for_port_127, GitDaemon};

/// AC1 — the interrupted-run regression: a stale orphan (leaked root +
/// pidfile + unheld owner lock + live `git daemon`) is reaped; the
/// daemon is dead and the root is gone.
#[test]
#[serial_test::serial]
fn reaps_orphaned_daemon_from_dead_run() {
    // Leak the root on purpose: nobody owns it — the faithful
    // interrupted-run shape.
    let root = TempDir::with_prefix("caduceus-origin-reap-")
        .expect("stale root")
        .keep();
    let gitroot = root.join("gitroot");
    fs::create_dir_all(&gitroot).expect("mkdir gitroot");

    // Lock file + owner guard, exactly like `GitDaemon::start`, then
    // simulate the owner's death: dropping the guard releases the
    // flock (kernel semantics) while the file itself remains.
    fs::File::create(root.join("git-daemon.lock")).expect("create lock");
    let lock_file = fs::File::open(root.join("git-daemon.lock")).expect("open lock");
    let guard = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock).expect("owner lock");

    let port = free_port_127();
    let log_path = root.join("git-daemon.log");
    let log = fs::File::create(&log_path).expect("create log");
    // Guard the child so a failure inside this test (readiness panic,
    // broken reaper) cannot leak a real daemon.
    let mut child = {
        struct KillOnDrop(Option<Child>);
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
                .expect("spawn git daemon"),
        ))
    };
    wait_for_port_127(port, Duration::from_secs(5));

    let pid = child.0.as_ref().expect("child present").id();
    fs::write(root.join("git-daemon.pid"), format!("{pid}\n")).expect("write pidfile");

    // Owner death: the flock is released when the guard and its file
    // are dropped. The daemon keeps running, reparented like a real
    // orphan.
    drop(guard);

    let pid = Pid::from_raw(pid as i32);
    assert!(
        kill(pid, None).is_ok(),
        "pre-reap sanity: the orphaned daemon must still be alive"
    );

    let mut child = child.0.take().expect("child present after readiness");
    let summary = reap_stale_git_daemons();

    // Outcome-based (not `daemons_killed == 1`): under the full
    // parallel gate a sibling binary's own start()-reap may legally
    // perform the kill first; the outcome is identical either way.
    // `try_wait` (not kill(pid, 0)) asserts death because the daemon
    // is our own child and must be reaped by this test.
    let deadline = std::time::SystemTime::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::SystemTime::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("reaped daemon did not die within 5s; summary {summary:?}");
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("try_wait after reap: {e}");
            }
        }
    };
    assert_eq!(
        status.signal(),
        Some(9),
        "daemon must be SIGKILLed (summary {summary:?})"
    );
    assert!(
        !root.exists(),
        "stale root must be removed by the reap (summary {summary:?})"
    );
}

/// AC2a — a stale root whose pidfile names a DEAD pid: nothing is
/// killed; the ownerless root (and its pidfile) is removed.
#[test]
#[serial_test::serial]
fn stale_pidfile_with_dead_pid_kills_nothing() {
    let root = TempDir::with_prefix("caduceus-origin-reap-dead-")
        .expect("stale root")
        .keep();
    fs::create_dir_all(root.join("gitroot")).expect("mkdir gitroot");

    // Lock file present but NEVER held: the owner died without ever
    // acquiring (or acquired and the kernel released the flock).
    fs::File::create(root.join("git-daemon.lock")).expect("create lock");

    // A pid that is definitely dead: spawn a child, reap it, use its
    // pid.
    let mut sleeper = Command::new("sleep").arg("1").spawn().expect("spawn sleep");
    let dead_pid = sleeper.id();
    let _ = sleeper.wait().expect("wait sleep");

    fs::write(root.join("git-daemon.pid"), format!("{dead_pid}\n")).expect("write pidfile");

    let summary = reap_stale_git_daemons();

    assert_eq!(
        summary.daemons_killed, 0,
        "a dead pid must never be counted as killed"
    );
    assert!(
        !root.join("git-daemon.pid").exists(),
        "stale pidfile must be removed"
    );
    assert!(
        !root.exists(),
        "ownerless root with a dead pid must be removed"
    );
}

/// AC2b — the critical safety negative: a pidfile pointing at a LIVE
/// process that is NOT a git daemon. The reaper must never signal it;
/// it drops the pidfile and leaves the root.
#[test]
#[serial_test::serial]
fn pidfile_with_unrelated_live_pid_never_kills() {
    let root = TempDir::with_prefix("caduceus-origin-reap-unrelated-")
        .expect("stale root")
        .keep();
    fs::create_dir_all(root.join("gitroot")).expect("mkdir gitroot");
    fs::File::create(root.join("git-daemon.lock")).expect("create lock");

    let mut sleeper = Command::new("sleep")
        .arg("300")
        .spawn()
        .expect("spawn sleep");
    let sleep_pid = sleeper.id();
    fs::write(root.join("git-daemon.pid"), format!("{sleep_pid}\n")).expect("write pidfile");

    let summary = reap_stale_git_daemons();

    assert_eq!(
        summary.daemons_killed, 0,
        "an unrelated live process must never be killed"
    );
    assert!(
        kill(Pid::from_raw(sleep_pid as i32), None).is_ok(),
        "the unrelated process must still be alive"
    );
    assert!(
        !root.join("git-daemon.pid").exists(),
        "the stale pidfile must be removed"
    );
    assert!(
        root.exists(),
        "an identity-mismatch root is left for forensics, not deleted"
    );
    assert!(
        sleeper.try_wait().expect("try_wait sleep").is_none(),
        "the sleep child must still be running"
    );

    // The reaper deliberately left the root, so the TEST must remove
    // it or it litters the shared temp dir (a later run's reap would
    // see a no-pidfile root: harmless, but dirty).
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    let _ = fs::remove_dir_all(&root);
}

/// AC2c — a LIVE test's root (lock held) is invisible to every
/// reaper: the daemon survives, its port still accepts, and the root
/// survives.
#[test]
#[serial_test::serial]
fn live_owned_root_is_never_reaped() {
    let daemon = GitDaemon::start("reap-live", "owner", "repo");

    let summary = reap_stale_git_daemons();
    assert_eq!(
        summary.daemons_killed, 0,
        "a live owner's daemon must never be killed"
    );

    let root = daemon.root().to_path_buf();
    assert!(root.exists(), "a live root must survive the reap");

    // The daemon's port still accepts connections (flock guard I5).
    let port: u16 = daemon
        .uri()
        .split(':')
        .nth(2)
        .and_then(|s| s.split('/').next())
        .expect("uri port")
        .parse()
        .expect("uri port parses");
    wait_for_port_127(port, Duration::from_secs(5));

    drop(daemon);
    assert!(!root.exists(), "normal Drop must still remove the root");
}

/// AC3 — pidfile lifecycle: written on start and removed on Drop
/// (with the root).
#[test]
#[serial_test::serial]
fn pidfile_written_on_start_removed_on_drop() {
    let daemon = GitDaemon::start("reap-lifecycle", "owner", "repo");

    let root = daemon.root().to_path_buf();
    let pidfile = root.join("git-daemon.pid");
    assert!(pidfile.exists(), "pidfile must exist after start");
    let pid: u32 = fs::read_to_string(&pidfile)
        .expect("read pidfile")
        .trim()
        .parse()
        .expect("pidfile parses");
    assert!(
        kill(Pid::from_raw(pid as i32), None).is_ok(),
        "the recorded pid must be a live process"
    );

    drop(daemon);
    assert!(
        !root.exists(),
        "drop must remove the root (pidfile with it)"
    );
}
