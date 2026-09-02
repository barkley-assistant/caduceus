//! Shared tempdir helper for the test suite (issue #269).
//!
//! Every test that needs a scratch directory under `std::env::temp_dir()`
//! must use one of these helpers instead of rolling its own. The old
//! per-file helpers derived uniqueness from
//! `SystemTime::now().duration_since(UNIX_EPOCH).as_nanos()`, which
//! collides on the coarse-clock macOS runner (~1 us resolution): two
//! parallel tests observe the same nanosecond, produce the same path,
//! and race on the same directory (proven torn git trees in PR #254).
#![allow(dead_code)] // not every test binary that includes fixtures uses both fns

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique scratch directory under `std::env::temp_dir()`.
///
/// Uniqueness derives from `std::process::id()` + a process-wide
/// monotonic `AtomicU64` counter — NEVER from the wall clock. The
/// macOS runner clock is coarse (~1 us) so two parallel tests can
/// observe the same nanosecond and collide; the pid + counter pair
/// is collision-free within a process and across parallel test
/// binaries (different pids).
///
/// No auto-cleanup. Matches the shape of the 50 local
/// `fn tempdir(label) -> PathBuf` helpers it replaces.
pub fn tempdir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "caduceus-test-{label}-{}-{}",
        std::process::id(),
        n
    ));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

/// `tempfile::TempDir` with auto-cleanup, for the one site that
/// previously returned `TempDir` (tests/state/migration_test.rs).
/// Uniqueness same as `tempdir(label)`; the `TempDir` owns the
/// directory and removes it on drop.
///
/// Mirrors the old migration_test.rs helper exactly: create a unique
/// parent, then `TempDir::new_in(parent)` (tempfile 3.x has no
/// `from_path`; the pre-fix helper used `new_in` on an `as_nanos`
/// parent, so this keeps the same shape with collision-free
/// uniqueness).
pub fn tempdir_owned(label: &str) -> tempfile::TempDir {
    let parent = tempdir(label);
    tempfile::TempDir::new_in(parent).expect("TempDir new_in")
}
