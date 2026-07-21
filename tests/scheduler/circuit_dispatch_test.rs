//! Integration tests for the circuit breaker dispatch hook.
//!
//! Tests verify that the circuit breaker interacts correctly with
//! the worker pool: open circuit → dispatch routes to NeedsAttention,
//! half-open → probe admitted, successful probe → circuit resets.

use std::sync::Arc;

use caduceus::scheduler::circuit::{CircuitConfig, CircuitStore};
use caduceus::scheduler::{DrainConfig, Pool};
use caduceus::FakeClock;

fn drain_config() -> DrainConfig {
    DrainConfig::from_seconds_and_ms(5, 100) // 5s drain, 100ms backpressure
}

fn circuit_store() -> (CircuitStore, FakeClock) {
    let conn = rusqlite::Connection::open_in_memory().expect("in-memory db");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS circuit_state (
            scope TEXT NOT NULL,
            scope_id TEXT NOT NULL,
            state TEXT NOT NULL DEFAULT 'closed',
            consecutive_failures INTEGER NOT NULL DEFAULT 0,
            last_failure_at INTEGER,
            opened_at INTEGER,
            last_probe_at INTEGER,
            PRIMARY KEY (scope, scope_id)
        );",
    )
    .expect("create circuit_state table");
    let config = CircuitConfig::test_defaults();
    let store = CircuitStore::new(conn, config);
    let clock = FakeClock::new();
    (store, clock)
}

#[tokio::test]
async fn closed_circuit_allows_pool_admission() {
    // GIVEN a closed circuit and a pool with capacity
    let (store, clock) = circuit_store();
    let pool = Arc::new(Pool::new(2, drain_config()));

    // WHEN the circuit is closed and admission is attempted
    let admit = store.try_admit("repository", "owner/repo", &clock).unwrap();

    // THEN the circuit allows admission
    assert!(
        matches!(
            admit,
            caduceus::scheduler::circuit::AdmissionResult::Admitted
                | caduceus::scheduler::circuit::AdmissionResult::NoCircuit
        ),
        "closed circuit should allow admission, got {:?}",
        admit
    );

    // AND pool admission succeeds
    let result = pool.admit("owner/repo").await;
    assert!(
        result.is_ok(),
        "pool admission should succeed: {:?}",
        result
    );
}

#[tokio::test]
async fn open_circuit_blocks_admission() {
    // GIVEN an open circuit and a pool with capacity
    let (store, clock) = circuit_store();
    let pool = Arc::new(Pool::new(2, drain_config()));

    // Open the circuit via infrastructure failures
    for _ in 0..3 {
        let _ = store
            .record_failure("repository", "owner/repo", &clock)
            .unwrap();
    }

    // WHEN the circuit is open
    let admit = store.try_admit("repository", "owner/repo", &clock).unwrap();

    // THEN the circuit rejects admission
    assert!(
        matches!(
            admit,
            caduceus::scheduler::circuit::AdmissionResult::CircuitOpen { .. }
        ),
        "open circuit should reject admission, got {:?}",
        admit
    );

    // Pool admission would still succeed (the circuit check is the gate)
    let result = pool.admit("owner/repo").await;
    assert!(
        result.is_ok(),
        "pool should still admit when circuit is open (dispatch hook checks circuit first)"
    );
}

#[tokio::test]
async fn half_open_probe_admitted() {
    // GIVEN an open circuit that transitions to half-open
    let (store, clock) = circuit_store();
    let pool = Arc::new(Pool::new(2, drain_config()));

    // Open the circuit
    for _ in 0..3 {
        let _ = store
            .record_failure("repository", "owner/repo", &clock)
            .unwrap();
    }

    // Advance past the open interval
    clock.advance(1800);

    // WHEN try_admit is called (this IS the probe)
    let admit = store.try_admit("repository", "owner/repo", &clock).unwrap();

    // THEN the probe is admitted
    assert_eq!(
        admit,
        caduceus::scheduler::circuit::AdmissionResult::Admitted,
        "half-open circuit should admit the probe"
    );

    // Pool admission should also succeed
    let result = pool.admit("owner/repo").await;
    assert!(
        result.is_ok(),
        "pool should admit after circuit probe: {:?}",
        result
    );
}

#[tokio::test]
async fn successful_probe_after_dispatch_resets_circuit() {
    // GIVEN a half-open circuit with a probe admitted
    let (store, clock) = circuit_store();
    let pool = Arc::new(Pool::new(2, drain_config()));

    // Open the circuit
    for _ in 0..3 {
        let _ = store
            .record_failure("repository", "owner/repo", &clock)
            .unwrap();
    }
    clock.advance(1800);
    let _ = store.try_admit("repository", "owner/repo", &clock).unwrap();

    // WHEN the probe succeeds (simulating a successful dispatch)
    store
        .record_probe_result("repository", "owner/repo", true, &clock)
        .unwrap();

    // THEN the circuit resets to Closed
    let record = store
        .get_record("repository", "owner/repo")
        .unwrap()
        .unwrap();
    assert_eq!(
        record.state,
        caduceus::scheduler::circuit::CircuitState::Closed
    );
    assert_eq!(record.consecutive_failures, 0);

    // AND subsequent dispatches succeed
    let admit = store.try_admit("repository", "owner/repo", &clock).unwrap();
    assert!(
        matches!(
            admit,
            caduceus::scheduler::circuit::AdmissionResult::Admitted
                | caduceus::scheduler::circuit::AdmissionResult::NoCircuit
        ),
        "reset circuit should allow admission, got {:?}",
        admit
    );

    let result = pool.admit("owner/repo").await;
    assert!(
        result.is_ok(),
        "pool admission should succeed after circuit reset: {:?}",
        result
    );
}

#[tokio::test]
async fn max_degraded_age_routes_to_needs_attention() {
    // GIVEN a circuit open for 24+ hours
    let (store, clock) = circuit_store();

    // Open the circuit
    for _ in 0..3 {
        let _ = store
            .record_failure("repository", "owner/repo", &clock)
            .unwrap();
    }

    // Advance 24 hours
    clock.advance(86400);

    // WHEN try_admit is called
    let admit = store.try_admit("repository", "owner/repo", &clock).unwrap();

    // THEN it returns MaxDegradedAgeExceeded
    assert_eq!(
        admit,
        caduceus::scheduler::circuit::AdmissionResult::MaxDegradedAgeExceeded,
        "circuit open for 24h should escalate to NeedsAttention"
    );

    // AND exhausted_to_needs_attention returns the entry
    let exhausted = store.exhausted_to_needs_attention(&clock).unwrap();
    assert_eq!(exhausted.len(), 1);
    assert_eq!(exhausted[0].scope_id, "owner/repo");
}
