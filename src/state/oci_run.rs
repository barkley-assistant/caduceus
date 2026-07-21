//! Container run state — per-run state row keyed by `run_id`.
//!
//! The [`ContainerRunRow`] and [`OciRunDao`] track the lifecycle of
//! each OCI container the daemon spawns. The state row is the
//! durability boundary for crash recovery: a crash between `create`
//! and `remove` leaves a row that the next reconciliation pass picks
//! up (AC-05).

use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;

use rusqlite::{params, Connection};

use crate::infra::error::{CaduceusError, CaduceusResult};

// ---------------------------------------------------------------------------
// OciLifecycleState
// ---------------------------------------------------------------------------

/// Typed state of an OCI container lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OciLifecycleState {
    /// Container has been created but not yet started.
    Created,
    /// Container is running.
    Running,
    /// Container exited with a code.
    Exited(i32),
    /// Container was stopped gracefully.
    Stopped,
    /// Container was killed.
    Killed,
    /// Container has been removed.
    Removed,
    /// Reconciliation is pending (engine was unavailable).
    PendingReconciliation,
}

impl fmt::Display for OciLifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OciLifecycleState::Created => write!(f, "Created"),
            OciLifecycleState::Running => write!(f, "Running"),
            OciLifecycleState::Exited(code) => write!(f, "Exited({code})"),
            OciLifecycleState::Stopped => write!(f, "Stopped"),
            OciLifecycleState::Killed => write!(f, "Killed"),
            OciLifecycleState::Removed => write!(f, "Removed"),
            OciLifecycleState::PendingReconciliation => write!(f, "PendingReconciliation"),
        }
    }
}

impl FromStr for OciLifecycleState {
    type Err = CaduceusError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Created" => Ok(OciLifecycleState::Created),
            "Running" => Ok(OciLifecycleState::Running),
            "Stopped" => Ok(OciLifecycleState::Stopped),
            "Killed" => Ok(OciLifecycleState::Killed),
            "Removed" => Ok(OciLifecycleState::Removed),
            "PendingReconciliation" => Ok(OciLifecycleState::PendingReconciliation),
            _ => {
                if let Some(code) = s.strip_prefix("Exited(").and_then(|s| s.strip_suffix(')')) {
                    let code: i32 = code.parse().map_err(|_| {
                        CaduceusError::Other(format!("invalid OciLifecycleState: {s:?}"))
                    })?;
                    return Ok(OciLifecycleState::Exited(code));
                }
                Err(CaduceusError::Other(format!(
                    "invalid OciLifecycleState: {s:?}"
                )))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ContainerRunRow
// ---------------------------------------------------------------------------

/// A single row in the `oci_runs` table.
#[derive(Clone, Debug)]
pub struct ContainerRunRow {
    pub run_id: String,
    pub container_id: Option<String>,
    pub state: OciLifecycleState,
    pub engine: String,
    pub created_at: String,
    pub updated_at: String,
    pub daemon_id: String,
    pub issue_id: String,
    pub worker_command_sha256: String,
}

// ---------------------------------------------------------------------------
// OciRunState trait
// ---------------------------------------------------------------------------

/// Trait abstracting the `oci_runs` table for the lifecycle module.
/// Production uses [`OciRunDao`]; tests use a fake.
pub trait OciRunState: Send + Sync {
    /// Insert a new container run row.
    fn insert(&self, row: &ContainerRunRow) -> CaduceusResult<()>;
    /// Update the state of an existing container run.
    fn update_state(&self, run_id: &str, state: &OciLifecycleState) -> CaduceusResult<()>;
    /// List all rows whose state is `PendingReconciliation`.
    fn list_pending_reconciliation(&self) -> CaduceusResult<Vec<ContainerRunRow>>;
    /// Get a single container run by its run_id.
    fn get(&self, run_id: &str) -> CaduceusResult<Option<ContainerRunRow>>;
    /// Delete a container run by run_id.
    fn delete(&self, run_id: &str) -> CaduceusResult<()>;
}

impl OciRunState for OciRunDao {
    fn insert(&self, row: &ContainerRunRow) -> CaduceusResult<()> {
        OciRunDao::insert(self, row)
    }

    fn update_state(&self, run_id: &str, state: &OciLifecycleState) -> CaduceusResult<()> {
        OciRunDao::update_state(self, run_id, state)
    }

    fn list_pending_reconciliation(&self) -> CaduceusResult<Vec<ContainerRunRow>> {
        OciRunDao::list_pending_reconciliation(self)
    }

    fn get(&self, run_id: &str) -> CaduceusResult<Option<ContainerRunRow>> {
        OciRunDao::get(self, run_id)
    }

    fn delete(&self, run_id: &str) -> CaduceusResult<()> {
        OciRunDao::delete(self, run_id)
    }
}

// ---------------------------------------------------------------------------
// OciRunDao
// ---------------------------------------------------------------------------

/// Data-access object for the `oci_runs` table.
pub struct OciRunDao {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for OciRunDao {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OciRunDao").finish()
    }
}

impl OciRunDao {
    /// Wrap an open SQLite connection.
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Insert a new container run row.
    pub fn insert(&self, row: &ContainerRunRow) -> CaduceusResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO oci_runs (run_id, container_id, state, engine, \
  created_at, updated_at, daemon_id, issue_id, \
  worker_command_sha256) \
  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                row.run_id,
                row.container_id,
                row.state.to_string(),
                row.engine,
                row.created_at,
                row.updated_at,
                row.daemon_id,
                row.issue_id,
                row.worker_command_sha256,
            ],
        )
        .map_err(|e| CaduceusError::StateCorrupt {
            path: ":memory:".into(),
            message: format!("oci_run insert: {e}"),
        })?;
        Ok(())
    }

    /// Update the state of an existing container run.
    pub fn update_state(&self, run_id: &str, state: &OciLifecycleState) -> CaduceusResult<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE oci_runs SET state = ?1, updated_at = ?2 WHERE run_id = ?3",
            params![state.to_string(), now, run_id],
        )
        .map_err(|e| CaduceusError::StateCorrupt {
            path: ":memory:".into(),
            message: format!("oci_run update_state: {e}"),
        })?;
        Ok(())
    }

    /// List all rows whose state is `PendingReconciliation`.
    pub fn list_pending_reconciliation(&self) -> CaduceusResult<Vec<ContainerRunRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT run_id, container_id, state, engine, created_at, \
  updated_at, daemon_id, issue_id, worker_command_sha256 \
  FROM oci_runs WHERE state = 'PendingReconciliation'",
            )
            .map_err(|e| CaduceusError::StateCorrupt {
                path: ":memory:".into(),
                message: format!("oci_run list_pending: {e}"),
            })?;
        let rows = stmt
            .query_map([], |r| {
                let state_str: String = r.get(2)?;
                let state: OciLifecycleState =
                    state_str.parse().unwrap_or(OciLifecycleState::Removed);
                Ok(ContainerRunRow {
                    run_id: r.get(0)?,
                    container_id: r.get(1)?,
                    state,
                    engine: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                    daemon_id: r.get(6)?,
                    issue_id: r.get(7)?,
                    worker_command_sha256: r.get(8)?,
                })
            })
            .map_err(|e| CaduceusError::StateCorrupt {
                path: ":memory:".into(),
                message: format!("oci_run list_pending query: {e}"),
            })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(|e| CaduceusError::StateCorrupt {
                path: ":memory:".into(),
                message: format!("oci_run list_pending row: {e}"),
            })?);
        }
        Ok(result)
    }

    /// Get a single container run by its run_id.
    pub fn get(&self, run_id: &str) -> CaduceusResult<Option<ContainerRunRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT run_id, container_id, state, engine, created_at, \
  updated_at, daemon_id, issue_id, worker_command_sha256 \
  FROM oci_runs WHERE run_id = ?1",
            )
            .map_err(|e| CaduceusError::StateCorrupt {
                path: ":memory:".into(),
                message: format!("oci_run get prepare: {e}"),
            })?;
        let mut rows = stmt
            .query_map(params![run_id], |r| {
                let state_str: String = r.get(2)?;
                let state: OciLifecycleState =
                    state_str.parse().unwrap_or(OciLifecycleState::Removed);
                Ok(ContainerRunRow {
                    run_id: r.get(0)?,
                    container_id: r.get(1)?,
                    state,
                    engine: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                    daemon_id: r.get(6)?,
                    issue_id: r.get(7)?,
                    worker_command_sha256: r.get(8)?,
                })
            })
            .map_err(|e| CaduceusError::StateCorrupt {
                path: ":memory:".into(),
                message: format!("oci_run get query: {e}"),
            })?;
        match rows.next() {
            Some(Ok(row)) => Ok(Some(row)),
            Some(Err(e)) => Err(CaduceusError::StateCorrupt {
                path: ":memory:".into(),
                message: format!("oci_run get row: {e}"),
            }),
            None => Ok(None),
        }
    }

    /// Delete a container run by run_id.
    pub fn delete(&self, run_id: &str) -> CaduceusResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM oci_runs WHERE run_id = ?1", params![run_id])
            .map_err(|e| CaduceusError::StateCorrupt {
                path: ":memory:".into(),
                message: format!("oci_run delete: {e}"),
            })?;
        Ok(())
    }
}
