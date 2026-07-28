//! Cross-process lease enforcement regression tests (issue-106).
//!
//! Validates that `Pool::admit` consults the shared `LeaseStore` before
//! granting a permit, so overlapping cron ticks on different processes
//! cannot exceed the configured `worker_parallelism` host-wide. Two
//! `Pool` instances sharing one `Arc<Mutex<LeaseStore>>` and the same
//! temp state_dir (real WAL SQLite via `state::store::open_in`) act as
//! the "two processes" pair: each pool would naively grant a permit
//! for its in-process semaphore, but the lease store enforces the
//! per-repo contract across both.
//!
//! Per the design (DEC-1, DEC-5), lease keys use the synthetic
//! `repo:<owner>/<repo>` prefix so the row is self-documenting in
//! `sqlite3` inspection. The two call sites in production tick pass
//! `format!("repo:{repo_key}")` as the first argument to
//! `Pool::admit`; the tests below mirror that construction.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use caduceus::infra::error::CaduceusError;
use caduceus::scheduler::{DrainConfig, LeaseStore, Pool};
use caduceus::state::store;

fn temp_state_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

fn open_store(dir: &std::path::Path) -> LeaseStore {
    let conn = store::open_in(dir).expect("open SQLite store");
    LeaseStore::new(conn)
}

fn drain_config() -> DrainConfig {
    DrainConfig::from_seconds_and_ms(5, 5_000)
}

/// Build the synthetic per-repo lease key. Mirrors the
/// `format!("repo:{repo_key}")` construction in the production
/// tick dispatch loop.
fn lease_key(repo_key: &str) -> String {
    format!("repo:{repo_key}")
}

#[tokio::test]
async fn two_pools_same_repo_second_admit_rejected() {
    // GIVEN two pools sharing one LeaseStore (the "two processes")
    let dir = temp_state_dir();
    let path: PathBuf = dir.path().to_path_buf();
    let store1 = Arc::new(Mutex::new(open_store(&path)));
    let store2 = Arc::new(Mutex::new(open_store(&path)));

    let pool1 = Pool::new(2, drain_config()).with_lease_store(store1, Duration::from_secs(60));
    let pool2 = Pool::new(2, drain_config()).with_lease_store(store2, Duration::from_secs(60));

    // WHEN pool1 admits "owner/repo" and pool2 tries the same repo
    let repo = "owner/repo";
    let key = lease_key(repo);
    let admit1 = pool1
        .admit(&key, repo)
        .await
        .expect("first admit must succeed");

    // THEN pool2 is rejected with PoolSaturated (lease contended)
    let result2 = pool2.admit(&key, repo).await;
    assert!(
        matches!(result2, Err(CaduceusError::PoolSaturated { .. })),
        "second admit for same repo must be rejected with PoolSaturated, got {:?}",
        result2
    );

    // Drop the held admission so the lease guard releases
    drop(admit1);
}

#[tokio::test]
async fn two_pools_different_repos_both_admit() {
    // GIVEN two pools sharing one LeaseStore
    let dir = temp_state_dir();
    let path: PathBuf = dir.path().to_path_buf();
    let store1 = Arc::new(Mutex::new(open_store(&path)));
    let store2 = Arc::new(Mutex::new(open_store(&path)));

    let pool1 = Pool::new(2, drain_config()).with_lease_store(store1, Duration::from_secs(60));
    let pool2 = Pool::new(2, drain_config()).with_lease_store(store2, Duration::from_secs(60));

    // WHEN two pools admit distinct repos
    let _admit1 = pool1
        .admit(&lease_key("owner/repo-a"), "owner/repo-a")
        .await
        .expect("first repo admit");
    let _admit2 = pool2
        .admit(&lease_key("owner/repo-b"), "owner/repo-b")
        .await
        .expect("second repo admit on different repo must succeed");
}

#[tokio::test]
async fn release_then_readmit_succeeds() {
    // GIVEN two pools sharing one LeaseStore
    let dir = temp_state_dir();
    let path: PathBuf = dir.path().to_path_buf();
    let store1 = Arc::new(Mutex::new(open_store(&path)));
    let store2 = Arc::new(Mutex::new(open_store(&path)));

    let pool1 = Pool::new(2, drain_config()).with_lease_store(store1, Duration::from_secs(60));
    let pool2 = Pool::new(2, drain_config()).with_lease_store(store2, Duration::from_secs(60));

    // WHEN pool1 holds the lease and pool2 is rejected
    let repo = "owner/repo";
    let key = lease_key(repo);
    let admit1 = pool1.admit(&key, repo).await.expect("first admit");
    assert!(pool2.admit(&key, repo).await.is_err());

    // AND pool1's admission is dropped (LeaseGuard RAII releases)
    drop(admit1);

    // THEN pool2 can now admit the same repo
    let _admit2 = pool2
        .admit(&key, repo)
        .await
        .expect("second admit after release must succeed");
}

#[tokio::test]
async fn lease_released_on_drop_with_panic_safety() {
    // GIVEN one pool with a lease store
    let dir = temp_state_dir();
    let path: PathBuf = dir.path().to_path_buf();
    let store1 = Arc::new(Mutex::new(open_store(&path)));

    let pool1 = Pool::new(2, drain_config()).with_lease_store(store1, Duration::from_secs(60));

    // WHEN the first admission goes out of scope (RAII Drop)
    {
        let repo = "owner/repo";
        let key = lease_key(repo);
        let _admit = pool1.admit(&key, repo).await.expect("first admit");
        // _admit dropped at end of this block
    }

    // THEN the same pool can admit the same repo again
    let _admit2 = pool1
        .admit(&lease_key("owner/repo"), "owner/repo")
        .await
        .expect("second admit must succeed after first dropped");
}
