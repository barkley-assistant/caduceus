//! Durable finalization checkpoints — the crash-safe record of
//! every FINAL-001 stage transition.
//!
//! Each checkpoint is written *before* its corresponding external
//! effect (commit, push, PR creation, comment, close). Recovery
//! reads the last durable checkpoint for a given `run_id` and
//! resumes from that stage.
//!
//! The backing table is the `checkpoints` table in the SQLite
//! state store (schema in [`crate::state::store`]).
//!
//! ## Contract (FINAL-001)
//!
//! The finalization sequence is:
//!
//! ```text
//! ResultValidated -> Committed -> Pushed -> PrCreated -> Commented
//!   -> AwaitingReview -> Done
//! ```
//!
//! Each checkpoint is committed before beginning the next
//! externally visible action. Recovery resumes from the last
//! durable checkpoint and uses idempotency keys or remote
//! reconciliation so a crash does not duplicate commits, pushes,
//! pull requests, comments, or issue updates.

use rusqlite::{params, Connection};

use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::state::queue::FinalizationStage;

/// A row from the `checkpoints` table.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckpointRow {
    /// Run identifier.
    pub run_id: String,
    /// The checkpoint stage (snake_case string, matches
    /// `FinalizationStage::as_str()`).
    pub stage: String,
    /// Optional JSON payload carried with the checkpoint
    /// (e.g. commit OID, PR number, remote markers).
    pub checkpoint_data: Option<String>,
    /// RFC-3339 timestamp of when the checkpoint was created.
    pub created_at: String,
}

impl CheckpointRow {
    /// Parse the `stage` field back into a [`FinalizationStage`].
    pub fn stage_enum(&self) -> Option<FinalizationStage> {
        FinalizationStage::from_str(&self.stage)
    }
}

/// Persist a checkpoint for the given `run_id` and `stage`.
///
/// The checkpoint is written with `INSERT OR REPLACE` so
/// re-persisting the same `(run_id, stage)` pair is idempotent
/// (the `created_at` is refreshed).
///
/// `checkpoint_data` is an optional JSON payload that carries
/// operation-specific markers (commit OID, PR number, etc.).
/// It must be `None` or a valid JSON string.
///
/// ## Crash guarantee
///
/// The checkpoint is written and committed *before* the caller
/// begins the corresponding external effect. If the daemon
/// crashes between this write and the external effect, recovery
/// resumes from *this* checkpoint and re-executes the effect
/// idempotently.
pub fn persist_checkpoint(
    conn: &Connection,
    run_id: &str,
    stage: FinalizationStage,
    checkpoint_data: Option<&str>,
) -> CaduceusResult<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let stage_str = stage.as_str();

    conn.execute(
        "INSERT OR REPLACE INTO checkpoints (run_id, stage, checkpoint_data, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![run_id, stage_str, checkpoint_data, now],
    )
    .map_err(|e| CaduceusError::StateCorrupt {
        path: std::path::PathBuf::from("<checkpoints>"),
        message: format!("cannot persist checkpoint ({run_id}, {stage_str}): {e}"),
    })?;

    Ok(())
}

/// Return all checkpoints for a given `run_id`, ordered by
/// `created_at` ascending.
///
/// Returns an empty vec when the run has no checkpoints.
pub fn checkpoint_for_run(conn: &Connection, run_id: &str) -> CaduceusResult<Vec<CheckpointRow>> {
    let mut stmt = conn
        .prepare("SELECT run_id, stage, checkpoint_data, created_at FROM checkpoints WHERE run_id = ?1 ORDER BY created_at ASC")
        .map_err(|e| CaduceusError::StateCorrupt {
            path: std::path::PathBuf::from("<checkpoints>"),
            message: format!("cannot prepare checkpoint query: {e}"),
        })?;

    let rows = stmt
        .query_map(params![run_id], |row| {
            Ok(CheckpointRow {
                run_id: row.get(0)?,
                stage: row.get(1)?,
                checkpoint_data: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .map_err(|e| CaduceusError::StateCorrupt {
            path: std::path::PathBuf::from("<checkpoints>"),
            message: format!("cannot query checkpoints: {e}"),
        })?;

    let collected: Vec<CheckpointRow> = rows.filter_map(|r| r.ok()).collect();

    Ok(collected)
}

/// Return the last (most recent) checkpoint for a given `run_id`,
/// or `None` if the run has no checkpoints.
pub fn last_checkpoint_for_run(
    conn: &Connection,
    run_id: &str,
) -> CaduceusResult<Option<CheckpointRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT run_id, stage, checkpoint_data, created_at FROM checkpoints \
             WHERE run_id = ?1 ORDER BY created_at DESC LIMIT 1",
        )
        .map_err(|e| CaduceusError::StateCorrupt {
            path: std::path::PathBuf::from("<checkpoints>"),
            message: format!("cannot prepare last-checkpoint query: {e}"),
        })?;

    let result = stmt
        .query_row(params![run_id], |row| {
            Ok(CheckpointRow {
                run_id: row.get(0)?,
                stage: row.get(1)?,
                checkpoint_data: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .ok();

    Ok(result)
}

/// Delete all checkpoints for a given `run_id`. Used when a
/// generation completes or is archived.
pub fn delete_checkpoints_for_run(conn: &Connection, run_id: &str) -> CaduceusResult<()> {
    conn.execute("DELETE FROM checkpoints WHERE run_id = ?1", params![run_id])
        .map_err(|e| CaduceusError::StateCorrupt {
            path: std::path::PathBuf::from("<checkpoints>"),
            message: format!("cannot delete checkpoints for run {run_id}: {e}"),
        })?;
    Ok(())
}

/// Delete a specific checkpoint for a given `(run_id, stage)`.
pub fn delete_checkpoint(
    conn: &Connection,
    run_id: &str,
    stage: FinalizationStage,
) -> CaduceusResult<()> {
    let stage_str = stage.as_str();
    conn.execute(
        "DELETE FROM checkpoints WHERE run_id = ?1 AND stage = ?2",
        params![run_id, stage_str],
    )
    .map_err(|e| CaduceusError::StateCorrupt {
        path: std::path::PathBuf::from("<checkpoints>"),
        message: format!("cannot delete checkpoint ({run_id}, {stage_str}): {e}"),
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::store;

    #[test]
    fn persist_and_read_back() {
        let path = std::env::temp_dir().join(format!("cp-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        let db = path.join("test.db");
        let conn = store::open(&db).expect("open db");

        let run_id = "unit-run-1";
        persist_checkpoint(&conn, run_id, FinalizationStage::Committed, None).expect("persist");
        persist_checkpoint(&conn, run_id, FinalizationStage::Pushed, None).expect("persist");

        let rows: Vec<CheckpointRow> = checkpoint_for_run(&conn, run_id).expect("query");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].stage, "committed");
        assert_eq!(rows[1].stage, "pushed");

        let last = last_checkpoint_for_run(&conn, run_id)
            .expect("query")
            .expect("must have last");
        assert_eq!(last.stage, "pushed");

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn last_checkpoint_is_none_for_empty_run() {
        let path = std::env::temp_dir().join(format!("cp-unit-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        let db = path.join("test.db");
        let conn = store::open(&db).expect("open db");

        let result = last_checkpoint_for_run(&conn, "no-such-run").expect("query");
        assert!(result.is_none(), "must be None for unknown run");

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn overwrite_same_stage() {
        let path = std::env::temp_dir().join(format!("cp-unit-over-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        let db = path.join("test.db");
        let conn = store::open(&db).expect("open db");

        let run_id = "unit-run-overwrite";
        persist_checkpoint(&conn, run_id, FinalizationStage::Pushed, Some(r#"{"v":1}"#))
            .expect("persist v1");
        persist_checkpoint(&conn, run_id, FinalizationStage::Pushed, Some(r#"{"v":2}"#))
            .expect("persist v2");

        let rows: Vec<CheckpointRow> = checkpoint_for_run(&conn, run_id).expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].checkpoint_data.as_deref(), Some(r#"{"v":2}"#));

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn delete_checkpoints_for_run_test() {
        let path = std::env::temp_dir().join(format!("cp-unit-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        let db = path.join("test.db");
        let conn = store::open(&db).expect("open db");

        let run_id = "unit-run-del";
        persist_checkpoint(&conn, run_id, FinalizationStage::Committed, None).expect("persist");
        persist_checkpoint(&conn, run_id, FinalizationStage::Pushed, None).expect("persist");

        super::delete_checkpoints_for_run(&conn, run_id).expect("delete");

        let rows: Vec<CheckpointRow> = checkpoint_for_run(&conn, run_id).expect("query");
        assert!(rows.is_empty(), "all checkpoints deleted");

        let _ = std::fs::remove_dir_all(&path);
    }
}
