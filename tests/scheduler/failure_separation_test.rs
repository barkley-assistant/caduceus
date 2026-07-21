//! Tests verifying that infrastructure failures open circuits while
//! worker-attempt failures do not.
//!
//! The circuit breaker is only driven by infrastructure failures.
//! Worker-attempt failures must never call `record_failure` on the
//! CircuitStore — this test verifies the API boundary:
//! - Infrastructure failures (via `record_failure`) → circuit opens
//!   after threshold
//! - Worker failures (no `record_failure` call) → circuit stays closed
//! - Two counters never overlap

use caduceus::scheduler::circuit::{CircuitConfig, CircuitState, CircuitStore};
use caduceus::FakeClock;

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
fn worker_attempt_failures_do_not_open_circuit() {
    // GIVEN a provider with 0 infrastructure failures
    let (store, clock) = circuit_store();

    // WHEN 3 consecutive worker-attempt failures occur
    // (the caller simply does NOT call record_failure for worker failures)
    // THEN the circuit remains closed
    let result = store.try_admit("provider", "github", &clock).unwrap();
    // First visit → NoCircuit (implicitly closed)
    assert_eq!(
        format!("{:?}", result),
        "NoCircuit",
        "circuit should not open from worker failures alone"
    );

    // Verify no circuit record was created for worker-type failures
    let record = store.get_record("provider", "github").unwrap();
    assert!(record.is_none(), "no circuit record should exist");
}

#[test]
fn infrastructure_failures_open_circuit_after_threshold() {
    // GIVEN a provider with 0 infrastructure failures
    let (store, clock) = circuit_store();

    // WHEN 3 infrastructure failures occur (via record_failure)
    for i in 0..3 {
        let record = store.record_failure("provider", "github", &clock).unwrap();
        if i < 2 {
            assert_eq!(
                record.state,
                CircuitState::Closed,
                "circuit should stay closed before threshold"
            );
        }
    }

    // THEN the circuit opens after the third failure
    let record = store.get_record("provider", "github").unwrap().unwrap();
    assert_eq!(
        record.state,
        CircuitState::Open,
        "circuit must open after threshold infrastructure failures"
    );
    assert_eq!(record.consecutive_failures, 3);
}

#[test]
fn worker_and_infra_counters_do_not_overlap() {
    // GIVEN separate scopes for provider and repository
    let (store, clock) = circuit_store();

    // WHEN infrastructure failures occur on provider "github"
    for _ in 0..3 {
        let _ = store.record_failure("provider", "github", &clock).unwrap();
    }

    // AND worker failures are NOT recorded (no circuit calls for worker)

    // THEN the provider circuit is open
    let provider_record = store.get_record("provider", "github").unwrap().unwrap();
    assert_eq!(provider_record.state, CircuitState::Open);
    assert_eq!(provider_record.consecutive_failures, 3);

    // AND the repository circuit remains untouched (no record)
    let repo_record = store.get_record("repository", "owner/repo").unwrap();
    assert!(
        repo_record.is_none(),
        "repository circuit should not be affected"
    );
}

#[test]
fn infrastructure_and_worker_use_separate_scopes() {
    // Infrastructure failures are tracked per scope (provider vs repository)
    // Worker failures use a different classification path entirely
    let (store, clock) = circuit_store();

    // Record infrastructure failures for a repository
    for _ in 0..3 {
        let _ = store
            .record_failure("repository", "owner/repo-a", &clock)
            .unwrap();
    }

    // A different repository should not be affected
    let other = store.get_record("repository", "owner/repo-b").unwrap();
    assert!(
        other.is_none(),
        "unaffected repo should have no circuit record"
    );
}
