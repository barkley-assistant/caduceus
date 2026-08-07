//! Crash-recovery regression tests for the investigation finalization
//! checkpoints (issue #120).
//!
//! The investigation path persists `InvestigationReady` before the
//! findings comment POST and `InvestigationCommented` after it
//! succeeds. These tests lock in `resume_from_checkpoint`'s stage
//! mapping for the two investigation stages: a crash before the
//! comment resumes at `InvestigationCommented` (re-post is idempotent),
//! a crash after it resumes at `Done`, and a fully checkpointed run is
//! `AlreadyDone`.

use caduceus::daemon::tick::resume::{resume_from_checkpoint, ResumeAction};
use caduceus::finalize::voice::generate_operation_id;
use caduceus::state::checkpoints::persist_checkpoint;
use caduceus::state::queue::FinalizationStage;

/// Create an in-memory SQLite connection with the `checkpoints` schema.
fn in_memory_conn() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
    conn.execute(
        "CREATE TABLE IF NOT EXISTS checkpoints (
            run_id          TEXT NOT NULL,
            stage           TEXT NOT NULL,
            checkpoint_data TEXT,
            created_at      TEXT NOT NULL,
            operation_id    TEXT,
            remote_marker   TEXT,
            PRIMARY KEY (run_id, stage)
        );",
        [],
    )
    .expect("create checkpoints table");
    conn
}

/// Persist a checkpoint with a deterministic operation_id and an
/// optional remote marker.
fn checkpoint_with_marker(
    conn: &rusqlite::Connection,
    run_id: &str,
    stage: FinalizationStage,
    marker: Option<&str>,
) {
    persist_checkpoint(
        conn,
        run_id,
        stage,
        None,
        Some(&generate_operation_id(run_id, stage.as_str())),
        marker,
    )
    .expect("persist checkpoint");
}

#[test]
fn investigation_ready_resumes_at_investigation_commented() {
    let conn = in_memory_conn();
    let run_id = "run-investigation-ready";

    // Crash before the findings comment: only InvestigationReady was
    // persisted. Recovery must re-post the comment (idempotent marker).
    checkpoint_with_marker(&conn, run_id, FinalizationStage::InvestigationReady, None);

    match resume_from_checkpoint(&conn, run_id).expect("resume") {
        ResumeAction::Skip(FinalizationStage::InvestigationCommented) => {}
        other => panic!("expected Skip(InvestigationCommented), got {other:?}"),
    }
}

#[test]
fn investigation_commented_resumes_at_done() {
    let conn = in_memory_conn();
    let run_id = "run-investigation-commented";

    // Crash after the findings comment: both investigation stages were
    // persisted. Recovery must finish the entry without re-posting.
    checkpoint_with_marker(&conn, run_id, FinalizationStage::InvestigationReady, None);
    checkpoint_with_marker(
        &conn,
        run_id,
        FinalizationStage::InvestigationCommented,
        None,
    );

    match resume_from_checkpoint(&conn, run_id).expect("resume") {
        ResumeAction::Skip(FinalizationStage::Done) => {}
        other => panic!("expected Skip(Done), got {other:?}"),
    }
}

#[test]
fn investigation_done_returns_already_done() {
    let conn = in_memory_conn();
    let run_id = "run-investigation-done";

    // The investigation run fully checkpointed through Done: no work
    // remains on recovery.
    checkpoint_with_marker(&conn, run_id, FinalizationStage::InvestigationReady, None);
    checkpoint_with_marker(
        &conn,
        run_id,
        FinalizationStage::InvestigationCommented,
        None,
    );
    checkpoint_with_marker(&conn, run_id, FinalizationStage::Done, None);

    match resume_from_checkpoint(&conn, run_id).expect("resume") {
        ResumeAction::AlreadyDone => {}
        other => panic!("expected AlreadyDone, got {other:?}"),
    }
}
