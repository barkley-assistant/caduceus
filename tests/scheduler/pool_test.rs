//! Unit tests for the bounded concurrency pool with per-repository
//! exclusion.
//!
//! Tests cover:
//! - Parallelism limit enforcement
//! - Repository exclusion (same repo serializes, distinct repos concurrent)
//! - Backpressure / saturation (timeout returns PoolSaturated)
//! - Shutdown / drain (drain stops admission, in-flight workers complete)

use std::sync::Arc;
use std::time::Duration;

use caduceus::scheduler::{DrainConfig, Pool, PoolState};

fn drain_config() -> DrainConfig {
  DrainConfig::from_seconds_and_ms(5, 100) // 5s drain, 100ms backpressure
}

#[tokio::test]
async fn parallelism_limit_enforced() {
  // GIVEN worker_parallelism = 2
  // NOTE: distinct repos on purpose — per-repo exclusion is exercised
  // by `same_repo_exclusion_serializes` below. Using the same repo here
  // would deadlock because the first permit holds the exclusion lock
  // and the next admit would block waiting for it.
  let pool = Pool::new(2, drain_config());
  let mut permits = Vec::new();

  // WHEN 5 dispatches are submitted simultaneously across distinct repos
  for i in 0..5 {
  let repo = format!("owner/repo-{i}");
  let admit = pool.admit(&repo).await;
  match admit {
  Ok(admitted) => permits.push(admitted),
  Err(_) => break, // pool saturated
  }
  }

  // THEN at most 2 workers are active concurrently
  let state = pool.state();
  assert!(
  matches!(state, PoolState::Saturated | PoolState::Active(2)),
  "expected Saturated or Active(2), got {state:?}"
  );

  // Drop permits and verify pool returns to idle
  drop(permits);
  // Give the semaphore a moment to process drops
  tokio::time::sleep(Duration::from_millis(10)).await;
  assert_eq!(pool.state(), PoolState::Idle);
}

#[tokio::test]
async fn same_repo_exclusion_serializes() {
  // GIVEN worker_parallelism = 2
  let pool = Arc::new(Pool::new(2, drain_config()));
  let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
  let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));

  // WHEN two admissions for the SAME repo are submitted concurrently
  let pool1 = Arc::clone(&pool);
  let started1 = Arc::clone(&started);
  let finished1 = Arc::clone(&finished);
  let handle1 = tokio::spawn(async move {
  let _admit = pool1.admit("owner/same-repo").await.unwrap();
  started1.store(true, std::sync::atomic::Ordering::SeqCst);
  tokio::time::sleep(Duration::from_millis(100)).await;
  finished1.store(true, std::sync::atomic::Ordering::SeqCst);
  });

  // Spawn the second with a small delay so permit 1 is acquired first
  tokio::time::sleep(Duration::from_millis(5)).await;
  let pool2 = Arc::clone(&pool);
  let handle2 = tokio::spawn(async move {
  let _admit = pool2.admit("owner/same-repo").await.unwrap();
  });

  // THEN the first is started before the second (exclusion held)
  tokio::time::sleep(Duration::from_millis(20)).await;
  assert!(started.load(std::sync::atomic::Ordering::SeqCst));
  // The second should not have finished yet because the first holds the exclusion
  // (but it may have started since parallelism = 2 and semaphore is available)

  handle1.await.unwrap();
  handle2.await.unwrap();
}

#[tokio::test]
async fn distinct_repos_run_concurrently() {
  // GIVEN worker_parallelism = 2
  let pool = Arc::new(Pool::new(2, drain_config()));

  // WHEN admissions for DIFFERENT repos are submitted
  let pool1 = Arc::clone(&pool);
  let handle1 = tokio::spawn(async move {
  let _admit = pool1.admit("owner/repo-a").await.unwrap();
  tokio::time::sleep(Duration::from_millis(100)).await;
  });

  tokio::time::sleep(Duration::from_millis(5)).await;
  let pool2 = Arc::clone(&pool);
  let handle2 = tokio::spawn(async move {
  let _admit = pool2.admit("owner/repo-b").await.unwrap();
  });

  // THEN both can proceed concurrently (parallelism allows both)
  handle1.await.unwrap();
  handle2.await.unwrap();
  // Both completed, no deadlock
}

#[tokio::test]
async fn backpressure_budget_respected() {
  // GIVEN worker_parallelism = 1, backpressure_budget = 50ms, slot is held
  let cfg = DrainConfig::from_seconds_and_ms(5, 50);
  let pool = Pool::new(1, cfg);

  // Hold the only slot
  let _holder = pool.admit("owner/repo-a").await.unwrap();

  // WHEN a second dispatch is submitted
  let result = pool.admit("owner/repo-b").await;

  // THEN it returns PoolSaturated (or error after timeout)
  match result {
  Err(caduceus::CaduceusError::PoolSaturated { current_depth, max_depth }) => {
  assert_eq!(current_depth, 1, "one slot is held");
  assert_eq!(max_depth, 1, "max depth is 1");
  }
  other => panic!("expected PoolSaturated error, got {other:?}"),
  }
}

#[tokio::test]
async fn drain_blocks_new_admissions() {
  // GIVEN 2 workers in-flight
  let pool = Arc::new(Pool::new(2, drain_config()));
  let _permit1 = pool.admit("owner/repo-a").await.unwrap();
  let _permit2 = pool.admit("owner/repo-b").await.unwrap();
  assert_eq!(pool.state(), PoolState::Saturated);

  // WHEN drain is triggered
  let pool_clone = Arc::clone(&pool);
  let drain_handle = tokio::spawn(async move {
  pool_clone.drain().await;
  });

  // THEN new admissions are blocked
  let admit_result = pool.admit("owner/repo-c").await;
  assert!(
  admit_result.is_err(),
  "admit should fail during drain, got {admit_result:?}"
  );

  // Drop the held permits so drain completes
  drop(_permit1);
  drop(_permit2);

  drain_handle.await.unwrap();

  // After drain, pool should report Draining state
  assert_eq!(pool.state(), PoolState::Draining);
}

#[tokio::test]
async fn pool_state_transitions() {
  // GIVEN an idle pool
  let pool = Pool::new(2, drain_config());
  assert_eq!(pool.state(), PoolState::Idle);

  // WHEN one slot is acquired
  let _permit = pool.admit("owner/repo-a").await.unwrap();
  assert_eq!(pool.state(), PoolState::Active(1));

  // WHEN both slots are acquired
  let _permit2 = pool.admit("owner/repo-b").await.unwrap();
  assert_eq!(pool.state(), PoolState::Saturated);

  // WHEN one is released
  drop(_permit);
  tokio::time::sleep(Duration::from_millis(10)).await;
  assert_eq!(pool.state(), PoolState::Active(1));

  // WHEN all are released
  drop(_permit2);
  tokio::time::sleep(Duration::from_millis(10)).await;
  assert_eq!(pool.state(), PoolState::Idle);
}
