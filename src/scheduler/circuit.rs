//! SQLite-backed circuit breaker that gates worker dispatch per
//! provider and per repository.
//!
//! Infrastructure failures (transport, API 5xx, pool saturation,
//! token resolution, etc.) drive circuit state transitions
//! independently of worker-attempt failures. The circuit is
//! persisted in the same SQLite store as the rest of the daemon
//! state, so it survives restarts.
//!
//! # Circuit states
//!
//! * **Closed** — normal operation. Failures increment a counter;
//!   at the threshold the circuit transitions to Open.
//! * **Open** — the circuit is rejecting admissions. After a
//!   configurable open interval it transitions to HalfOpen for a
//!   single probe.
//! * **HalfOpen** — exactly one probe admission is permitted. If
//!   the probe succeeds the circuit resets to Closed; if it fails
//!   the circuit returns to Open.
//!
//! # Backoff
//!
//! The backoff schedule is a fixed list of delays (default
//! [30, 120, 600] seconds). The index into the schedule is
//! `min(consecutive_failures - threshold, backoff.len() - 1)` so
//! the last delay repeats indefinitely.

use std::str::FromStr;

use rusqlite::{params, Connection};

use crate::daemon::orchestration::Clock;
use crate::infra::error::{CaduceusError, CaduceusResult};

// ---------------------------------------------------------------------------
// CircuitState
// ---------------------------------------------------------------------------

/// The state of a single circuit breaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — failures are counted.
    Closed,
    /// Rejecting admissions — the circuit is open.
    Open,
    /// A single probe is permitted.
    HalfOpen,
}

impl CircuitState {
    /// Canonical string representation for the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            CircuitState::Closed => "closed",
            CircuitState::Open => "open",
            CircuitState::HalfOpen => "half_open",
        }
    }
}

impl std::fmt::Display for CircuitState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CircuitState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "closed" => Ok(CircuitState::Closed),
            "open" => Ok(CircuitState::Open),
            "half_open" => Ok(CircuitState::HalfOpen),
            other => Err(format!("unknown circuit state: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// CircuitScope
// ---------------------------------------------------------------------------

/// The scope of a circuit breaker — either a provider or a
/// repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CircuitScope {
    /// Provider-level circuit (e.g. "github").
    Provider,
    /// Repository-level circuit (e.g. "owner/repo").
    Repository,
}

impl CircuitScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            CircuitScope::Provider => "provider",
            CircuitScope::Repository => "repository",
        }
    }
}

impl std::fmt::Display for CircuitScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CircuitScope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "provider" => Ok(CircuitScope::Provider),
            "repository" => Ok(CircuitScope::Repository),
            other => Err(format!("unknown circuit scope: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// CircuitConfig
// ---------------------------------------------------------------------------

/// Configuration for a circuit breaker.
#[derive(Clone, Debug)]
pub struct CircuitConfig {
    /// Number of consecutive failures before the circuit opens.
    /// Default 3.
    pub failure_threshold: u32,
    /// Exponential backoff stages in seconds. Default [30, 120, 600].
    pub backoff_seconds: Vec<u64>,
    /// Seconds after which an open circuit transitions to half-open
    /// for a probe. Default 1800 (30 min).
    pub open_interval_seconds: u64,
    /// Maximum seconds a circuit can remain open before the work is
    /// escalated to NeedsAttention. Default 86400 (24h).
    pub max_degraded_seconds: u64,
}

impl CircuitConfig {
    /// Build a `CircuitConfig` from the fields in the daemon's
    /// [`Config`](crate::infra::config::Config).
    pub fn from_config(cfg: &crate::infra::config::Config) -> Self {
        Self {
            failure_threshold: cfg.circuit_failure_threshold,
            backoff_seconds: cfg.circuit_backoff_seconds.clone(),
            open_interval_seconds: cfg.circuit_open_interval_seconds,
            max_degraded_seconds: cfg.circuit_max_degraded_seconds,
        }
    }

    /// SCHED-002 defaults for testing.
    pub fn test_defaults() -> Self {
        Self {
            failure_threshold: 3,
            backoff_seconds: vec![30, 120, 600],
            open_interval_seconds: 1800,
            max_degraded_seconds: 86400,
        }
    }
}

// ---------------------------------------------------------------------------
// CircuitRecord
// ---------------------------------------------------------------------------

/// A row from the `circuit_state` table.
#[derive(Clone, Debug)]
pub struct CircuitRecord {
    pub scope: String,
    pub scope_id: String,
    pub state: CircuitState,
    pub consecutive_failures: u32,
    pub last_failure_at: Option<i64>,
    pub opened_at: Option<i64>,
    pub last_probe_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// AdmissionResult
// ---------------------------------------------------------------------------

/// The outcome of a [`CircuitStore::try_admit`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionResult {
    /// Admission is permitted — the circuit is closed or a probe
    /// is being admitted.
    Admitted,
    /// The circuit is open. `retry_after` seconds must elapse
    /// before the next attempt. `probe_in_flight` is true when a
    /// half-open probe is already in progress.
    CircuitOpen {
        retry_after: i64,
        probe_in_flight: bool,
    },
    /// The circuit has been open longer than `max_degraded_seconds`.
    /// The work must transition to NeedsAttention.
    MaxDegradedAgeExceeded,
    /// No circuit record exists for this scope/scope_id — this is
    /// the first visit, implicitly closed.
    NoCircuit,
}

// ---------------------------------------------------------------------------
// ExhaustedEntry
// ---------------------------------------------------------------------------

/// A circuit that has been open longer than the max degraded age.
#[derive(Clone, Debug)]
pub struct ExhaustedEntry {
    pub scope: String,
    pub scope_id: String,
    pub consecutive_failures: u32,
    pub last_failure_at: Option<i64>,
    pub opened_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// CircuitStore
// ---------------------------------------------------------------------------

/// SQLite-backed circuit breaker store.
///
/// Wraps a [`rusqlite::Connection`] and a [`CircuitConfig`]. All
/// mutations use atomic SQLite transactions so concurrent ticks
/// are serialised naturally.
pub struct CircuitStore {
    conn: rusqlite::Connection,
    config: CircuitConfig,
}

impl std::fmt::Debug for CircuitStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitStore")
            .field("config", &self.config)
            .finish()
    }
}

impl CircuitStore {
    /// Open a circuit store on the given connection with the given
    /// config. The caller is responsible for ensuring the
    /// `circuit_state` table exists (migration v4).
    pub fn new(conn: rusqlite::Connection, config: CircuitConfig) -> Self {
        Self { conn, config }
    }

    /// Record an infrastructure failure for the given scope and
    /// scope_id. Returns the updated [`CircuitRecord`].
    ///
    /// If `consecutive_failures` reaches the threshold, the circuit
    /// transitions to Open and `opened_at` is recorded.
    pub fn record_failure(
        &self,
        scope: &str,
        scope_id: &str,
        clock: &dyn Clock,
    ) -> CaduceusResult<CircuitRecord> {
        let now = clock.now_unix();
        let threshold = self.config.failure_threshold as i64;

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| CaduceusError::Other(format!("circuit tx start: {e}")))?;

        // Upsert: increment consecutive_failures, set last_failure_at.
        tx.execute(
            "INSERT INTO circuit_state (scope, scope_id, state, consecutive_failures, last_failure_at)
             VALUES (?1, ?2, 'closed', 1, ?3)
             ON CONFLICT(scope, scope_id) DO UPDATE SET
               consecutive_failures = consecutive_failures + 1,
               last_failure_at = ?3",
            params![scope, scope_id, now],
        )
        .map_err(|e| CaduceusError::Other(format!("circuit upsert failure: {e}")))?;

        // Read back the current state.
        let record = Self::read_record(&tx, scope, scope_id)?;

        // Check threshold: if consecutive_failures >= threshold,
        // transition to Open.
        if record.consecutive_failures as i64 >= threshold && record.state == CircuitState::Closed {
            tx.execute(
                "UPDATE circuit_state SET state = 'open', opened_at = ?1
                 WHERE scope = ?2 AND scope_id = ?3",
                params![now, scope, scope_id],
            )
            .map_err(|e| CaduceusError::Other(format!("circuit open transition: {e}")))?;
        }

        tx.commit()
            .map_err(|e| CaduceusError::Other(format!("circuit tx commit: {e}")))?;

        Self::read_record(&self.conn, scope, scope_id)
    }

    /// Try to admit an entry for the given scope and scope_id.
    ///
    /// Returns [`AdmissionResult::Admitted`] when the circuit is
    /// closed, or when a half-open probe is being admitted. Returns
    /// [`AdmissionResult::CircuitOpen`] when the circuit is open
    /// and the open interval has not elapsed. Returns
    /// [`AdmissionResult::MaxDegradedAgeExceeded`] when the circuit
    /// has been open longer than the max degraded age.
    pub fn try_admit(
        &self,
        scope: &str,
        scope_id: &str,
        clock: &dyn Clock,
    ) -> CaduceusResult<AdmissionResult> {
        let now = clock.now_unix();

        let record = match Self::read_record(&self.conn, scope, scope_id) {
            Ok(r) => r,
            Err(_) => return Ok(AdmissionResult::NoCircuit),
        };

        match record.state {
            CircuitState::Closed => Ok(AdmissionResult::Admitted),
            CircuitState::Open => {
                let opened_at = record.opened_at.unwrap_or(now);
                let open_interval = self.config.open_interval_seconds as i64;
                let max_degraded = self.config.max_degraded_seconds as i64;

                // Check max degraded age first.
                if now >= opened_at + max_degraded {
                    return Ok(AdmissionResult::MaxDegradedAgeExceeded);
                }

                if now >= opened_at + open_interval {
                    // Transition to HalfOpen for a probe.
                    let tx = self
                        .conn
                        .unchecked_transaction()
                        .map_err(|e| CaduceusError::Other(format!("circuit tx start: {e}")))?;
                    tx.execute(
                        "UPDATE circuit_state SET state = 'half_open', last_probe_at = ?1
                         WHERE scope = ?2 AND scope_id = ?3",
                        params![now, scope, scope_id],
                    )
                    .map_err(|e| {
                        CaduceusError::Other(format!("circuit half-open transition: {e}"))
                    })?;
                    tx.commit()
                        .map_err(|e| CaduceusError::Other(format!("circuit tx commit: {e}")))?;
                    Ok(AdmissionResult::Admitted)
                } else {
                    let retry_after = opened_at + open_interval - now;
                    Ok(AdmissionResult::CircuitOpen {
                        retry_after,
                        probe_in_flight: false,
                    })
                }
            }
            CircuitState::HalfOpen => {
                // Check if a probe is already in flight.
                let record = Self::read_record(&self.conn, scope, scope_id)?;
                // If the probe was recently set, treat as in-flight.
                // We use a small window: if last_probe_at is within
                // the last 5 seconds, consider it a probe in flight.
                // This is a heuristic to prevent multiple concurrent
                // probes on the same circuit.
                if let Some(last_probe) = record.last_probe_at {
                    if now - last_probe < 5 {
                        return Ok(AdmissionResult::CircuitOpen {
                            retry_after: 5 - (now - last_probe),
                            probe_in_flight: true,
                        });
                    }
                }
                // This IS the probe.
                let tx = self
                    .conn
                    .unchecked_transaction()
                    .map_err(|e| CaduceusError::Other(format!("circuit tx start: {e}")))?;
                tx.execute(
                    "UPDATE circuit_state SET last_probe_at = ?1
                     WHERE scope = ?2 AND scope_id = ?3",
                    params![now, scope, scope_id],
                )
                .map_err(|e| CaduceusError::Other(format!("circuit probe mark: {e}")))?;
                tx.commit()
                    .map_err(|e| CaduceusError::Other(format!("circuit tx commit: {e}")))?;
                Ok(AdmissionResult::Admitted)
            }
        }
    }

    /// Record the result of a probe.
    ///
    /// If `success` is true, the circuit resets to Closed with
    /// `consecutive_failures = 0`. If `success` is false, the
    /// circuit returns to Open with `opened_at` updated.
    pub fn record_probe_result(
        &self,
        scope: &str,
        scope_id: &str,
        success: bool,
        clock: &dyn Clock,
    ) -> CaduceusResult<()> {
        let now = clock.now_unix();

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| CaduceusError::Other(format!("circuit tx start: {e}")))?;

        if success {
            tx.execute(
                "UPDATE circuit_state SET state = 'closed', consecutive_failures = 0,
                 opened_at = NULL, last_probe_at = NULL, last_failure_at = NULL
                 WHERE scope = ?1 AND scope_id = ?2",
                params![scope, scope_id],
            )
            .map_err(|e| CaduceusError::Other(format!("circuit probe success: {e}")))?;
        } else {
            tx.execute(
                "UPDATE circuit_state SET state = 'open', opened_at = ?1,
                 consecutive_failures = consecutive_failures + 1, last_failure_at = ?1
                 WHERE scope = ?2 AND scope_id = ?3",
                params![now, scope, scope_id],
            )
            .map_err(|e| CaduceusError::Other(format!("circuit probe failure: {e}")))?;
        }

        tx.commit()
            .map_err(|e| CaduceusError::Other(format!("circuit tx commit: {e}")))?;

        Ok(())
    }

    /// Find all circuits that have been open longer than the max
    /// degraded age. Returns a list of [`ExhaustedEntry`] values.
    pub fn exhausted_to_needs_attention(
        &self,
        clock: &dyn Clock,
    ) -> CaduceusResult<Vec<ExhaustedEntry>> {
        let now = clock.now_unix();
        let max_age = self.config.max_degraded_seconds as i64;

        let mut stmt = self
            .conn
            .prepare(
                "SELECT scope, scope_id, consecutive_failures, last_failure_at, opened_at
                 FROM circuit_state
                 WHERE opened_at IS NOT NULL AND opened_at + ?1 <= ?2",
            )
            .map_err(|e| CaduceusError::Other(format!("circuit exhausted query: {e}")))?;

        let rows = stmt
            .query_map(params![max_age, now], |row| {
                Ok(ExhaustedEntry {
                    scope: row.get(0)?,
                    scope_id: row.get(1)?,
                    consecutive_failures: row.get(2)?,
                    last_failure_at: row.get(3)?,
                    opened_at: row.get(4)?,
                })
            })
            .map_err(|e| CaduceusError::Other(format!("circuit exhausted map: {e}")))?;

        let mut entries = Vec::new();
        for row in rows {
            entries.push(
                row.map_err(|e| CaduceusError::Other(format!("circuit exhausted row: {e}")))?,
            );
        }

        Ok(entries)
    }

    /// Look up a single circuit record.
    pub fn get_record(&self, scope: &str, scope_id: &str) -> CaduceusResult<Option<CircuitRecord>> {
        let result = Self::read_record(&self.conn, scope, scope_id);
        match result {
            Ok(r) => Ok(Some(r)),
            Err(_) => Ok(None),
        }
    }

    /// Borrow the underlying connection (for testing).
    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn read_record(
        conn: &Connection,
        scope: &str,
        scope_id: &str,
    ) -> CaduceusResult<CircuitRecord> {
        let record = conn
            .query_row(
                "SELECT scope, scope_id, state, consecutive_failures,
                        last_failure_at, opened_at, last_probe_at
                 FROM circuit_state
                 WHERE scope = ?1 AND scope_id = ?2",
                params![scope, scope_id],
                |row| {
                    let state_str: String = row.get(2)?;
                    let state: CircuitState = state_str.parse().map_err(|e: String| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
                    })?;
                    Ok(CircuitRecord {
                        scope: row.get(0)?,
                        scope_id: row.get(1)?,
                        state,
                        consecutive_failures: row.get(3)?,
                        last_failure_at: row.get(4)?,
                        opened_at: row.get(5)?,
                        last_probe_at: row.get(6)?,
                    })
                },
            )
            .map_err(|e| CaduceusError::Other(format!("circuit read record: {e}")))?;
        Ok(record)
    }
}

// ---------------------------------------------------------------------------
// Inline tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod inline_tests {
    use super::*;
    use crate::daemon::orchestration::FakeClock;

    fn circuit_store() -> (CircuitStore, FakeClock) {
        let conn = Connection::open_in_memory().expect("in-memory db");
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

    #[test]
    fn threshold_opens_circuit() {
        // GIVEN a closed circuit for repository "owner/repo"
        let (store, clock) = circuit_store();

        // WHEN 3 consecutive failures occur
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

        let r3 = store
            .record_failure("repository", "owner/repo", &clock)
            .unwrap();
        // THEN the circuit opens
        assert_eq!(r3.state, CircuitState::Open);
        assert_eq!(r3.consecutive_failures, 3);
        assert!(r3.opened_at.is_some());
    }

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
    fn half_open_transition_after_open_interval() {
        // GIVEN an open circuit
        let (store, clock) = circuit_store();
        for _ in 0..3 {
            let _ = store
                .record_failure("repository", "owner/repo", &clock)
                .unwrap();
        }

        // WHEN the open interval elapses
        clock.advance(1800);

        // THEN try_admit transitions to HalfOpen and admits
        let result = store.try_admit("repository", "owner/repo", &clock).unwrap();
        assert_eq!(result, AdmissionResult::Admitted);

        // Verify the circuit is now HalfOpen
        let record = store
            .get_record("repository", "owner/repo")
            .unwrap()
            .unwrap();
        assert_eq!(record.state, CircuitState::HalfOpen);
    }

    #[test]
    fn successful_probe_resets_circuit() {
        // GIVEN an open circuit that transitions to HalfOpen
        let (store, clock) = circuit_store();
        for _ in 0..3 {
            let _ = store
                .record_failure("repository", "owner/repo", &clock)
                .unwrap();
        }
        clock.advance(1800);
        let _ = store.try_admit("repository", "owner/repo", &clock).unwrap();

        // WHEN the probe succeeds
        store
            .record_probe_result("repository", "owner/repo", true, &clock)
            .unwrap();

        // THEN the circuit resets to Closed
        let record = store
            .get_record("repository", "owner/repo")
            .unwrap()
            .unwrap();
        assert_eq!(record.state, CircuitState::Closed);
        assert_eq!(record.consecutive_failures, 0);
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

    #[test]
    fn twentyfour_hour_age_triggers_escalation() {
        // GIVEN an open circuit
        let (store, clock) = circuit_store();
        for _ in 0..3 {
            let _ = store
                .record_failure("repository", "owner/repo", &clock)
                .unwrap();
        }

        // WHEN 24 hours elapse
        clock.advance(86400);

        // THEN try_admit returns MaxDegradedAgeExceeded
        let result = store.try_admit("repository", "owner/repo", &clock).unwrap();
        assert_eq!(result, AdmissionResult::MaxDegradedAgeExceeded);

        // AND exhausted_to_needs_attention returns the entry
        let exhausted = store.exhausted_to_needs_attention(&clock).unwrap();
        assert_eq!(exhausted.len(), 1);
        assert_eq!(exhausted[0].scope_id, "owner/repo");
    }

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

    #[test]
    fn restart_preserves_state() {
        // GIVEN an open circuit saved to persistent SQLite
        let dir = std::env::temp_dir().join(format!("circuit-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("circuit.db");

        let config = CircuitConfig::test_defaults();
        let clock = FakeClock::new();

        {
            let conn = Connection::open(&db_path).expect("open db");
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

        // WHEN the connection is closed and a new one opens
        {
            let conn = Connection::open(&db_path).expect("re-open db");
            let store = CircuitStore::new(conn, config);

            // THEN the circuit state is preserved
            let record = store
                .get_record("repository", "owner/repo")
                .unwrap()
                .unwrap();
            assert_eq!(record.state, CircuitState::Open);
            assert_eq!(record.consecutive_failures, 3);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
