//! Regression tests for the bounded-concurrency admission guard.
//!
//! These tests exercise the `Pool::admit` contract directly and
//! complement the binding fix in `src/daemon/tick/mod.rs` by
//! documenting the cap and capacity-restoration semantics.

use std::time::Duration;

use caduceus::scheduler::{DrainConfig, Pool, PoolState};

fn drain_config() -> DrainConfig {
    DrainConfig::from_seconds_and_ms(5, 100) // 5s drain, 100ms backpressure
}

#[tokio::test]
async fn admission_guard_enforces_parallelism_cap() {
    // GIVEN worker_parallelism = 2 and two slots are already held
    let pool = Pool::new(2, drain_config());
    let _admit_a = pool.admit("owner/repo-a").await.unwrap();
    let _admit_b = pool.admit("owner/repo-b").await.unwrap();

    // THEN the pool reports itself as saturated
    assert_eq!(pool.state(), PoolState::Saturated);

    // AND a third admit on a distinct repo is rejected with the
    // expected saturation error.
    let err = pool.admit("owner/repo-c").await.unwrap_err();
    assert!(
        matches!(
            err,
            caduceus::CaduceusError::PoolSaturated {
                current_depth: 2,
                max_depth: 2,
            }
        ),
        "expected PoolSaturated {{ current_depth: 2, max_depth: 2 }}, got {err}"
    );
}

#[tokio::test]
async fn admission_guard_restores_capacity_on_drop() {
    // GIVEN worker_parallelism = 2 and two slots are held
    let pool = Pool::new(2, drain_config());
    let admit_a = pool.admit("owner/repo-a").await.unwrap();
    let admit_b = pool.admit("owner/repo-b").await.unwrap();
    assert_eq!(pool.state(), PoolState::Saturated);

    // WHEN the first admission is dropped
    drop(admit_a);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(pool.state(), PoolState::Active(1));

    // WHEN the second admission is dropped
    drop(admit_b);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(pool.state(), PoolState::Idle);
}
