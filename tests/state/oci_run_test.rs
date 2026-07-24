//! Tests for the oci_runs SQLite migration v5 and ContainerRunRow DAO.

use caduceus::state::oci_run::{ContainerRunRow, OciLifecycleState, OciRunDao};

/// Helper: open an in-memory SQLite database with the oci_runs schema
/// applied (as the v5 migration would).
fn dao() -> OciRunDao {
    let conn = rusqlite::Connection::open_in_memory().expect("in-memory db");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS oci_runs (
  run_id  TEXT PRIMARY KEY,
  container_id  TEXT,
  state  TEXT NOT NULL,
  engine  TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  daemon_id  TEXT NOT NULL,
  issue_id  TEXT NOT NULL,
  worker_command_sha256 TEXT NOT NULL
  );
  CREATE INDEX IF NOT EXISTS idx_oci_runs_container_id ON oci_runs(container_id);
  CREATE INDEX IF NOT EXISTS idx_oci_runs_daemon_id ON oci_runs(daemon_id);
  CREATE INDEX IF NOT EXISTS idx_oci_runs_state ON oci_runs(state);",
    )
    .expect("create oci_runs table");
    OciRunDao::new(conn)
}

fn sample_row(run_id: &str, state: OciLifecycleState) -> ContainerRunRow {
    ContainerRunRow {
        run_id: run_id.to_string(),
        container_id: Some("ctr-abc123".to_string()),
        state,
        engine: "docker".to_string(),
        created_at: "2026-07-21T00:00:00Z".to_string(),
        updated_at: "2026-07-21T00:00:00Z".to_string(),
        daemon_id: "daemon-01".to_string(),
        issue_id: "owner/repo#1".to_string(),
        worker_command_sha256: "abcdef".to_string(),
    }
}

// insert_oci_run_row

#[test]
fn insert_oci_run_row() {
    let dao = dao();
    let row = sample_row("run-001", OciLifecycleState::Created);
    dao.insert(&row).expect("insert must succeed");

    // Verify by reading back
    let loaded = dao
        .get("run-001")
        .expect("get must succeed")
        .expect("row must exist");
    assert_eq!(loaded.run_id, "run-001");
    assert_eq!(loaded.state, OciLifecycleState::Created);
    assert_eq!(loaded.engine, "docker");
}

// update_oci_run_state

#[test]
fn update_oci_run_state() {
    let dao = dao();
    let row = sample_row("run-002", OciLifecycleState::Created);
    dao.insert(&row).expect("insert");

    dao.update_state("run-002", &OciLifecycleState::Running)
        .expect("update state");

    let loaded = dao.get("run-002").expect("get").expect("must exist");
    assert_eq!(loaded.state, OciLifecycleState::Running);
}

// list_pending_reconciliation_returns_only_pending

#[test]
fn list_pending_reconciliation_returns_only_pending() {
    let dao = dao();

    // Insert rows in various states
    dao.insert(&sample_row("run-010", OciLifecycleState::Removed))
        .expect("insert removed");
    dao.insert(&sample_row(
        "run-011",
        OciLifecycleState::PendingReconciliation,
    ))
    .expect("insert pending");
    dao.insert(&sample_row("run-012", OciLifecycleState::Running))
        .expect("insert running");
    dao.insert(&sample_row(
        "run-013",
        OciLifecycleState::PendingReconciliation,
    ))
    .expect("insert pending 2");

    let pending = dao.list_pending_reconciliation().expect("list pending");
    assert_eq!(pending.len(), 2, "must return exactly 2 pending rows");
    let ids: Vec<&str> = pending.iter().map(|r| r.run_id.as_str()).collect();
    assert!(ids.contains(&"run-011"), "must contain run-011");
    assert!(ids.contains(&"run-013"), "must contain run-013");
}

// get_oci_run_by_id

#[test]
fn get_oci_run_by_id() {
    let dao = dao();
    dao.insert(&sample_row("run-020", OciLifecycleState::Exited(0)))
        .expect("insert");

    let row = dao.get("run-020").expect("get").expect("must exist");
    assert_eq!(row.run_id, "run-020");
    assert_eq!(row.state, OciLifecycleState::Exited(0));

    // Non-existent returns None
    let missing = dao.get("run-999").expect("get missing");
    assert!(missing.is_none(), "non-existent run must return None");
}

// delete_oci_run

#[test]
fn delete_oci_run() {
    let dao = dao();
    dao.insert(&sample_row("run-030", OciLifecycleState::Removed))
        .expect("insert");

    dao.delete("run-030").expect("delete must succeed");
    let row = dao.get("run-030").expect("get after delete");
    assert!(row.is_none(), "deleted row must not exist");

    // Deleting a non-existent row is a no-op (not an error)
    dao.delete("run-999")
        .expect("delete non-existent must not error");
}
