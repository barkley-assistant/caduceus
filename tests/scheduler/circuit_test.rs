//! Unit tests for the circuit breaker store with FakeClock.
//!
//! Tests cover:
//! - Threshold: 3 consecutive failures → Open
//! - Half-open: after 30 min, circuit moves to HalfOpen, one probe admitted
//! - Recovery: successful probe → Closed, failures reset
//! - 24h age: circuit open for 24h → exhausted_to_needs_attention returns it
//! - Restart: open circuit, save to SQLite, close connection, reopen, verify state preserved

use caduceus::scheduler::circuit::{
    AdmissionResult, CircuitConfig, CircuitScope, CircuitState, CircuitStore,
};
use caduceus::FakeClock;

/// Create an in-memory circuit store with the `circuit_state` table
/// and a FakeClock at epoch 0.
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

#[test]
fn threshold_three_failures_opens_circuit() {
    let (store, clock) = circuit_store();

    // 2 failures → circuit stays closed
    let r1 = store
        .record_failure("repository", "owner/repo", &clock)
        .unwrap();
    assert_eq!(r1.state, CircuitState::Closed);
    assert_eq!(r1.consecutive_failures, 1);

    let r2 = store
        .record_failure("repository", "owner/repo", &clock)
        .unwrap();
    assert_eq!(r2.state, CircuitState::Closed);
    assert_eq!(r2.consecutive_failures, 2);

    // 3rd failure → circuit opens
    let r3 = store
        .record_failure("repository", "owner/repo", &clock)
        .unwrap();
    assert_eq!(r3.state, CircuitState::Open);
    assert_eq!(r3.consecutive_failures, 3);
    assert!(r3.opened_at.is_some(), "opened_at must be set");
}

#[test]
fn half_open_after_thirty_minutes() {
    let (store, clock) = circuit_store();

    // Open the circuit
    for _ in 0..3 {
        let _ = store
            .record_failure("repository", "owner/repo", &clock)
            .unwrap();
    }

    // Advance past the open interval (30 min = 1800s)
    clock.advance(1800);

    // Now try_admit should transition to HalfOpen and admit
    let result = store.try_admit("repository", "owner/repo", &clock).unwrap();
    assert_eq!(result, AdmissionResult::Admitted);

    let record = store
        .get_record("repository", "owner/repo")
        .unwrap()
        .unwrap();
    assert_eq!(record.state, CircuitState::HalfOpen);
}

#[test]
fn recovery_on_successful_probe() {
    let (store, clock) = circuit_store();

    // Open the circuit
    for _ in 0..3 {
        let _ = store
            .record_failure("repository", "owner/repo", &clock)
            .unwrap();
    }
    clock.advance(1800);
    let _ = store.try_admit("repository", "owner/repo", &clock).unwrap();

    // Successful probe resets the circuit
    store
        .record_probe_result("repository", "owner/repo", true, &clock)
        .unwrap();

    let record = store
        .get_record("repository", "owner/repo")
        .unwrap()
        .unwrap();
    assert_eq!(record.state, CircuitState::Closed);
    assert_eq!(record.consecutive_failures, 0);
}

#[test]
fn twentyfour_hour_age_escalation() {
    let (store, clock) = circuit_store();

    // Open the circuit
    for _ in 0..3 {
        let _ = store
            .record_failure("repository", "owner/repo", &clock)
            .unwrap();
    }

    // Advance 24 hours
    clock.advance(86400);

    // try_admit should return MaxDegradedAgeExceeded
    let result = store.try_admit("repository", "owner/repo", &clock).unwrap();
    assert_eq!(result, AdmissionResult::MaxDegradedAgeExceeded);

    // exhausted_to_needs_attention should return the entry
    let exhausted = store.exhausted_to_needs_attention(&clock).unwrap();
    assert_eq!(exhausted.len(), 1);
    assert_eq!(exhausted[0].scope_id, "owner/repo");
    assert_eq!(exhausted[0].consecutive_failures, 3);
}

#[test]
fn restart_preserves_state() {
    let dir = std::env::temp_dir().join(format!("circuit-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("circuit.db");

    let config = CircuitConfig::test_defaults();
    let clock = FakeClock::new();

    // Open circuit and save to persistent SQLite
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
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
        .expect("create table");
        let store = CircuitStore::new(conn, config.clone());
        for _ in 0..3 {
            let _ = store
                .record_failure("repository", "owner/repo", &clock)
                .unwrap();
        }
    }

    // Close connection and reopen
    {
        let conn = rusqlite::Connection::open(&db_path).expect("re-open db");
        let store = CircuitStore::new(conn, config);

        let record = store
            .get_record("repository", "owner/repo")
            .unwrap()
            .unwrap();
        assert_eq!(record.state, CircuitState::Open);
        assert_eq!(record.consecutive_failures, 3);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// State and scope round trips (moved from src/scheduler/circuit.rs inline tests)

#[test]
fn circuit_state_round_trip() {
    assert_eq!(CircuitState::Closed.as_str(), "closed");
    assert_eq!(CircuitState::Open.as_str(), "open");
    assert_eq!(CircuitState::HalfOpen.as_str(), "half_open");
    assert_eq!(
        "closed".parse::<CircuitState>().unwrap(),
        CircuitState::Closed
    );
    assert_eq!("open".parse::<CircuitState>().unwrap(), CircuitState::Open);
    assert_eq!(
        "half_open".parse::<CircuitState>().unwrap(),
        CircuitState::HalfOpen
    );
    assert!("invalid".parse::<CircuitState>().is_err());
}

#[test]
fn circuit_scope_round_trip() {
    assert_eq!(CircuitScope::Provider.as_str(), "provider");
    assert_eq!(CircuitScope::Repository.as_str(), "repository");
    assert_eq!(
        "provider".parse::<CircuitScope>().unwrap(),
        CircuitScope::Provider
    );
    assert_eq!(
        "repository".parse::<CircuitScope>().unwrap(),
        CircuitScope::Repository
    );
    assert!("invalid".parse::<CircuitScope>().is_err());
}

// Admission paths (moved from src/scheduler/circuit.rs inline tests)

#[test]
fn closed_circuit_admits() {
    // GIVEN a closed circuit
    let (store, clock) = circuit_store();

    // WHEN try_admit is called
    let result = store.try_admit("repository", "owner/repo", &clock).unwrap();

    // THEN admission is granted
    assert_eq!(result, AdmissionResult::NoCircuit);
}

#[test]
fn open_circuit_rejects_admission() {
    // GIVEN an open circuit (3 failures)
    let (store, clock) = circuit_store();
    for _ in 0..3 {
        let _ = store
            .record_failure("repository", "owner/repo", &clock)
            .unwrap();
    }

    // WHEN try_admit is called immediately
    let result = store.try_admit("repository", "owner/repo", &clock).unwrap();

    // THEN admission is rejected with CircuitOpen
    match result {
        AdmissionResult::CircuitOpen {
            retry_after,
            probe_in_flight,
        } => {
            assert!(retry_after > 0);
            assert!(!probe_in_flight);
        }
        other => panic!("expected CircuitOpen, got {other:?}"),
    }
}

#[test]
fn failed_probe_reopens_circuit() {
    // GIVEN an open circuit that transitions to HalfOpen
    let (store, clock) = circuit_store();
    for _ in 0..3 {
        let _ = store
            .record_failure("repository", "owner/repo", &clock)
            .unwrap();
    }
    clock.advance(1800);
    let _ = store.try_admit("repository", "owner/repo", &clock).unwrap();

    // WHEN the probe fails
    store
        .record_probe_result("repository", "owner/repo", false, &clock)
        .unwrap();

    // THEN the circuit returns to Open
    let record = store
        .get_record("repository", "owner/repo")
        .unwrap()
        .unwrap();
    assert_eq!(record.state, CircuitState::Open);
    assert!(record.opened_at.is_some());
}

// Escalation boundary (moved from src/scheduler/circuit.rs inline tests)

#[test]
fn open_less_than_24h_does_not_escalate() {
    // GIVEN an open circuit
    let (store, clock) = circuit_store();
    for _ in 0..3 {
        let _ = store
            .record_failure("repository", "owner/repo", &clock)
            .unwrap();
    }

    // WHEN only 5 minutes elapse (less than open interval of 30 min)
    clock.advance(300);

    // THEN try_admit does NOT return MaxDegradedAgeExceeded
    let result = store.try_admit("repository", "owner/repo", &clock).unwrap();
    match result {
        AdmissionResult::CircuitOpen { .. } => {} // expected
        other => panic!("expected CircuitOpen, got {other:?}"),
    }

    // AND exhausted_to_needs_attention returns empty
    let exhausted = store.exhausted_to_needs_attention(&clock).unwrap();
    assert!(exhausted.is_empty());
}
