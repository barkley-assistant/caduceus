//! Leadership contention test (T-9).
//!
//! Verifies that two threads racing for the scheduler leadership
//! lock yield exactly one winner and one `LeadershipContended`
//! error.

use std::sync::{Arc, Barrier};
use std::thread;

use caduceus::infra::error::CaduceusError;
use caduceus::scheduler::LeaderToken;

fn temp_state_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

#[test]
fn two_threads_racing_yield_exactly_one_winner() {
    let dir = temp_state_dir();
    let state_dir = dir.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));

    let dir1 = state_dir.clone();
    let barrier1 = Arc::clone(&barrier);
    let h1 = thread::spawn(move || {
        barrier1.wait();
        LeaderToken::try_acquire(&dir1)
    });

    let dir2 = state_dir.clone();
    let barrier2 = Arc::clone(&barrier);
    let h2 = thread::spawn(move || {
        barrier2.wait();
        LeaderToken::try_acquire(&dir2)
    });

    let r1 = h1.join().expect("thread 1 panicked");
    let r2 = h2.join().expect("thread 2 panicked");

    // At least one thread must succeed.
    let t1 = r1.expect("thread 1 got non-contention error").is_some();
    let t2 = r2.expect("thread 2 got non-contention error").is_some();
    assert!(
        t1 || t2,
        "at least one thread must acquire the scheduler lock"
    );

    // Exactly one thread must succeed (the other gets None or an
    // error). When both threads race on the same lock file,
    // only one can hold the exclusive flock.
    assert!(
        !(t1 && t2),
        "only one thread may hold the scheduler lock at a time"
    );
}

#[test]
fn contention_returns_leadership_contended_error() {
    let dir = temp_state_dir();
    let state_dir = dir.path();

    // Hold the lock in the main thread.
    let _token = LeaderToken::try_acquire(state_dir)
        .expect("first acquire must succeed")
        .expect("first acquire must return Some");

    // A second attempt must return Ok(None) (non-blocking
    // contention) or Err(LeadershipContended).
    let result = LeaderToken::try_acquire(state_dir);
    match result {
        Ok(None) => {
            // Non-blocking contention — the lock is already held.
        }
        Err(CaduceusError::LeadershipContended { .. }) => {
            // Contention surfaced as an error.
        }
        other => {
            panic!(
                "expected Ok(None) or Err(LeadershipContended), got {:?}",
                other
            );
        }
    }
}

#[test]
fn with_lock_returns_contended_error_when_lock_held() {
    let dir = temp_state_dir();
    let state_dir = dir.path();

    // Hold the lock.
    let _token = LeaderToken::try_acquire(state_dir)
        .expect("first acquire")
        .expect("must be Some");

    // `with_lock` should fail with LeadershipContended.
    let result = LeaderToken::with_lock(state_dir, || Ok(()));
    assert!(
        matches!(result, Err(CaduceusError::LeadershipContended { .. })),
        "with_lock must return LeadershipContended when lock is held, got {:?}",
        result
    );
}

#[test]
fn lock_is_released_on_drop() {
    let dir = temp_state_dir();
    let state_dir = dir.path();

    // Acquire and immediately drop.
    {
        let _token = LeaderToken::try_acquire(state_dir)
            .expect("first acquire")
            .expect("must be Some");
    }

    // After drop, a second acquire must succeed.
    let token = LeaderToken::try_acquire(state_dir)
        .expect("second acquire")
        .expect("must be Some after drop");
    drop(token);
}

#[test]
fn with_lock_succeeds_when_lock_is_free() {
    let dir = temp_state_dir();
    let state_dir = dir.path();

    let result = LeaderToken::with_lock(state_dir, || Ok(42));
    assert_eq!(result.expect("with_lock result"), 42);
}
