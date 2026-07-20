//! Integration tests for worker pool drain.
//!
//! Tests cover:
//! - Direct async drain with in-flight workers
//! - Drain blocks new admissions
//! - Drain completes when workers finish

use std::sync::Arc;
use std::time::Duration;

use caduceus::scheduler::{DrainConfig, Pool, PoolState};

fn drain_config() -> DrainConfig {
  DrainConfig::from_seconds_and_ms(10, 100) // 10s drain, 100ms backpressure
}

#[tokio::test]
async fn drain_completes_when_workers_finish() {
  // GIVEN 2 workers are in-flight
  let pool = Arc::new(Pool::new(2, drain_config()));
  let _p1 = pool.admit("owner/repo-a").await.unwrap();
  let _p2 = pool.admit("owner/repo-b").await.unwrap();

  // Start drain in background
  let pool_clone = Arc::clone(&pool);
  let drain_handle = tokio::spawn(async move {
  pool_clone.drain().await
  });

  // Drain should be waiting — new admissions are blocked
  let admit_result = pool.admit("owner/repo-c").await;
  assert!(admit_result.is_err(), "admit should fail during drain");

  // Release the permits so drain can complete
  drop(_p1);
  drop(_p2);

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
  // GIVEN one in-flight worker
  let pool = Arc::new(Pool::new(2, drain_config()));
  let _p1 = pool.admit("owner/repo-a").await.unwrap();

  // Start drain
  let pool_clone = Arc::clone(&pool);
  let drain_handle = tokio::spawn(async move {
  pool_clone.drain().await
  });

  // New admissions are rejected during drain
  let err = pool.admit("owner/repo-new").await.unwrap_err();
  assert!(
  matches!(&err, caduceus::CaduceusError::PoolSaturated { .. }),
  "expected PoolSaturated during drain, got {err}"
  );

  // Release worker so drain completes
  drop(_p1);
  drain_handle.await.unwrap();
}
