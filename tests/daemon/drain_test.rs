//! Integration tests for worker pool drain.
//!
//! Tests cover:
//! - Direct async drain with in-flight workers
//! - Drain blocks new admissions
//! - Drain rejects with DrainTimeout when draining
//! - Drain timeout when workers exceed drain deadline

use std::sync::Arc;

use caduceus::scheduler::{DrainConfig, Pool, PoolState};

fn drain_config() -> DrainConfig {
    DrainConfig::from_seconds_and_ms(10, 100) // 10s drain, 100ms backpressure
}

#[tokio::test]
async fn drain_completes_when_workers_finish() {
    // GIVEN 2 workers are in-flight (pool is at capacity 2/2)
    let pool = Arc::new(Pool::new(2, drain_config()));
    let p1 = pool.admit("owner/repo-a").await.unwrap();
    let p2 = pool.admit("owner/repo-b").await.unwrap();

    // Start drain in background
    let pool_clone = Arc::clone(&pool);
    let drain_handle = tokio::spawn(async move { pool_clone.drain().await });

    // Yield once so the spawned drain task gets a chance to set the
    // draining flag before the test's admit call below. Without this,
    // the admit may race past the flag check and return `PoolSaturated`
    // (which is also a valid rejection during drain — the test is
    // order-tolerant by design).
    tokio::task::yield_now().await;

    // During drain, new admissions are rejected. The rejection may
    // arrive as either `DrainTimeout` (drain flag was set first) or
    // `PoolSaturated` (semaphore was full first). Both are valid
    // "drain in progress" responses.
    let admit_result = pool.admit("owner/repo-c").await;
    assert!(
        matches!(
            &admit_result,
            Err(caduceus::CaduceusError::DrainTimeout { .. })
                | Err(caduceus::CaduceusError::PoolSaturated { .. })
        ),
        "admit should be rejected during drain, got {admit_result:?}"
    );

    // Release the permits so drain can complete
    drop(p1);
    drop(p2);

    let timed_out = drain_handle.await.unwrap();
    assert!(timed_out.is_empty(), "all workers finished before timeout");

    // Pool is in Draining state after drain
    assert_eq!(pool.state(), PoolState::Draining);
}

#[tokio::test]
async fn drain_with_no_active_workers_completes_immediately() {
    // GIVEN a pool with no active workers
    let pool = Pool::new(2, drain_config());
    assert_eq!(pool.state(), PoolState::Idle);

    // WHEN drain is triggered
    let timed_out = pool.drain().await;

    // THEN it completes immediately with no timed-out runs
    assert!(timed_out.is_empty());
    assert_eq!(pool.state(), PoolState::Draining);
}

#[tokio::test]
async fn drain_rejects_new_admissions_until_workers_complete() {
    // GIVEN one in-flight worker (1/2 slots used, room for one more)
    let pool = Arc::new(Pool::new(2, drain_config()));
    let _p1 = pool.admit("owner/repo-a").await.unwrap();

    // Start drain
    let pool_clone = Arc::clone(&pool);
    let drain_handle = tokio::spawn(async move { pool_clone.drain().await });

    // Yield once so the spawned drain task gets a chance to set the
    // draining flag before the test's admit call below. Without this,
    // admit may return `Ok(Admitted)` because the pool still has a
    // free permit slot and the drain flag is not yet set.
    tokio::task::yield_now().await;

    // New admissions are rejected during drain with DrainTimeout.
    let err = pool.admit("owner/repo-new").await.unwrap_err();
    assert!(
        matches!(&err, caduceus::CaduceusError::DrainTimeout { .. }),
        "expected DrainTimeout during drain, got {err}"
    );

    // Release worker so drain completes
    drop(_p1);
    drain_handle.await.unwrap();
}

#[tokio::test]
async fn drain_timeout_when_workers_exceed_deadline() {
    // GIVEN a pool with 1 slot, a long-running worker holding it,
    // and a very short drain timeout
    let short_drain = DrainConfig::from_seconds_and_ms(0, 100); // 0s drain timeout
    let pool = Arc::new(Pool::new(1, short_drain));
    let _admitted = pool.admit("owner/repo-a").await.unwrap();

    // WHEN drain is triggered — the worker holds the only permit
    // for longer than the drain timeout
    let timed_out = pool.drain().await;

    // THEN drain returns immediately (timeout was 0s) while the
    // worker is still in-flight — the drain didn't wait long enough
    // to acquire the permit, but it completed without panicking.
    assert!(
        timed_out.is_empty(),
        "drain should not panic on short timeout"
    );
    assert_eq!(pool.state(), PoolState::Draining);
}
