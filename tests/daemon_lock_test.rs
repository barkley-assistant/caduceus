//! Task 3.2 acceptance tests for the daemon-wide `flock`.
//!
//! `DaemonLock::try_acquire` is a nonblocking exclusive lock on
//! `<state_dir>/daemon.lock`. CONTRACTS.md invariant #1 says a
//! second cron invocation exits 0 without polling or claiming,
//! which is what the lock guarantees. These tests cover:
//!
//! * Two concurrent `try_acquire` calls — only one returns `Some`.
//! * The lock is released on `Drop` so the second attempt wins
//!   when the first is dropped.
//! * The lock file may remain on disk after release.
//! * Two subprocesses competing for the same lock yield exactly
//!   one winner.
//! * `try_acquire` creates the state dir if missing (cron cold-start).
//! * `try_acquire` does not block the caller (a held lock returns
//!   `Ok(None)` immediately).

#![allow(unused_variables, unused_imports)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use caduceus::DaemonLock;

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-daemon-lock-test-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

// ---------------------------------------------------------------------------
// Single-process behaviour
// ---------------------------------------------------------------------------

#[test]
fn try_acquire_returns_some_on_first_call() {
    let dir = tempdir("first-some");
    let lock = DaemonLock::try_acquire(&dir).expect("try_acquire");
    assert!(lock.is_some(), "first attempt must succeed");
}

#[test]
fn try_acquire_returns_none_while_held() {
    let dir = tempdir("held");
    let first = DaemonLock::try_acquire(&dir).expect("first").expect("some");
    let second = DaemonLock::try_acquire(&dir).expect("second");
    assert!(
        second.is_none(),
        "second attempt while held must return None"
    );
    drop(first);
}

#[test]
fn lock_releases_on_drop() {
    let dir = tempdir("drop");
    {
        let first = DaemonLock::try_acquire(&dir).expect("first").expect("some");
        let second = DaemonLock::try_acquire(&dir).expect("second");
        assert!(second.is_none());
        drop(first);
    }
    // After dropping the first lock, a fresh attempt must succeed.
    let third = DaemonLock::try_acquire(&dir).expect("third");
    assert!(third.is_some(), "lock must be released after drop");
}

#[test]
fn lock_file_may_remain_on_disk_after_release() {
    // The contract says "the file itself may remain" — the OS
    // releases the flock when the file descriptor drops, but the
    // file does not have to be unlinked.
    let dir = tempdir("file-remains");
    let lock_path = dir.join("daemon.lock");
    {
        let _ = DaemonLock::try_acquire(&dir)
            .expect("acquire")
            .expect("some");
        assert!(lock_path.is_file(), "lock file created");
    }
    assert!(
        lock_path.is_file(),
        "lock file is allowed to remain after release"
    );
}

#[test]
fn try_acquire_does_not_block_when_held() {
    let dir = tempdir("nonblock");
    let first = DaemonLock::try_acquire(&dir).expect("first").expect("some");
    let start = Instant::now();
    let second = DaemonLock::try_acquire(&dir).expect("second");
    let elapsed = start.elapsed();
    assert!(second.is_none());
    // fs2 surfaces contention as EWOULDBLOCK; the call must
    // return within a small bound. 200ms is generous — the
    // underlying syscall is nonblocking.
    assert!(
        elapsed < Duration::from_millis(200),
        "try_acquire blocked for {elapsed:?}"
    );
    drop(first);
}

#[test]
fn try_acquire_creates_state_dir_if_missing() {
    // Cron cold-start: the state directory may not exist yet.
    // `try_acquire` must create it (and the daemon.lock file
    // inside it) without error.
    let dir = tempdir("cold-start");
    let nested = dir.join("state").join("nested");
    assert!(!nested.exists());
    let lock = DaemonLock::try_acquire(&nested).expect("try_acquire");
    assert!(lock.is_some());
    assert!(nested.is_dir());
    assert!(nested.join("daemon.lock").is_file());
}

// ---------------------------------------------------------------------------
// Multi-thread contention
// ---------------------------------------------------------------------------

#[test]
fn threads_yield_exactly_one_winner() {
    let dir = tempdir("threads");
    let n_threads = 16;
    let barrier = Arc::new(Barrier::new(n_threads));
    let mut handles = Vec::new();
    for _ in 0..n_threads {
        let barrier = Arc::clone(&barrier);
        let dir = dir.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            DaemonLock::try_acquire(&dir).expect("try_acquire")
        }));
    }
    let mut winners = 0usize;
    let mut held = Vec::new();
    for h in handles {
        if let Some(lock) = h.join().unwrap() {
            winners += 1;
            held.push(lock);
        }
    }
    assert_eq!(winners, 1, "exactly one thread wins");
    drop(held);
}

// ---------------------------------------------------------------------------
// Multi-process contention via subprocess
// ---------------------------------------------------------------------------

#[test]
fn two_subprocesses_yield_one_winner() {
    use std::io::Read;
    let dir = tempdir("subprocesses");
    let lock_path = dir.join("daemon.lock");
    let helper = env!("CARGO_BIN_EXE_daemon_lock_helper");
    let child_a = Command::new(helper)
        .arg("try-once")
        .arg(&lock_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn A");
    let child_b = Command::new(helper)
        .arg("try-once")
        .arg(&lock_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn B");
    // Wait both to finish so we can read their stdout.
    let out_a = child_a.wait_with_output().expect("wait A");
    let out_b = child_b.wait_with_output().expect("wait B");
    assert!(out_a.status.success(), "helper A: {:?}", out_a);
    assert!(out_b.status.success(), "helper B: {:?}", out_b);
    let mut stdout_a = String::new();
    let mut stdout_b = String::new();
    out_a.stdout.as_slice().read_to_string(&mut stdout_a).ok();
    out_b.stdout.as_slice().read_to_string(&mut stdout_b).ok();
    // Exactly one of the two helpers must observe the lock.
    let winners = [&stdout_a, &stdout_b]
        .iter()
        .filter(|s| s.contains("WON"))
        .count();
    assert_eq!(
        winners, 1,
        "expected exactly one winner; A={stdout_a:?} B={stdout_b:?}"
    );
    let losers = [&stdout_a, &stdout_b]
        .iter()
        .filter(|s| s.contains("LOST"))
        .count();
    assert_eq!(
        losers, 1,
        "expected exactly one loser; A={stdout_a:?} B={stdout_b:?}"
    );
}

#[test]
fn helper_holds_then_releases() {
    // The helper holds the lock for 250ms, then releases. A
    // second try_acquire from the parent test must succeed
    // either immediately or within 300ms of the helper exit.
    let dir = tempdir("hold-then-release");
    let lock_path = dir.join("daemon.lock");
    let helper = env!("CARGO_BIN_EXE_daemon_lock_helper");
    let mut child = Command::new(helper)
        .arg("hold-and-release")
        .arg(&lock_path)
        .arg("250")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    // Give the helper a moment to take the lock.
    thread::sleep(Duration::from_millis(50));
    let start = Instant::now();
    // While the helper holds the lock, the test's try_acquire
    // must return None immediately.
    let held = DaemonLock::try_acquire(lock_path.parent().unwrap()).expect("while-held");
    assert!(held.is_none(), "lock is held by helper");
    let _ = start.elapsed();
    // After the helper exits, the lock is released.
    let status = child.wait().expect("wait");
    assert!(status.success(), "helper exit: {status:?}");
    let after = DaemonLock::try_acquire(lock_path.parent().unwrap()).expect("after");
    assert!(after.is_some(), "lock released after helper exit");
}
