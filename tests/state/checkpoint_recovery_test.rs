//! Crash-recovery regression tests for finalization checkpoints.
//!
//! Each test simulates a crash immediately after an external effect
//! succeeds but before the matching checkpoint is written, then asserts
//! that `resume_from_checkpoint` resumes at the correct next stage.

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
fn crash_before_committed_checkpoint_resumes_at_committed() {
    let conn = in_memory_conn();
    let run_id = "run-crash-before-committed";

    // Only the ResultValidated checkpoint was written before the crash.
    checkpoint_with_marker(&conn, run_id, FinalizationStage::ResultValidated, None);

    match resume_from_checkpoint(&conn, run_id).expect("resume") {
        ResumeAction::Skip(FinalizationStage::Committed) => {}
        other => panic!("expected Skip(Committed), got {other:?}"),
    }
}

#[test]
fn committed_checkpoint_with_real_marker_resumes_at_pushed() {
    let conn = in_memory_conn();
    let run_id = "run-committed-marker";

    // The commit effect succeeded and its checkpoint was written.
    checkpoint_with_marker(&conn, run_id, FinalizationStage::Committed, Some("abc123"));

    match resume_from_checkpoint(&conn, run_id).expect("resume") {
        ResumeAction::Skip(FinalizationStage::Pushed) => {}
        other => panic!("expected Skip(Pushed), got {other:?}"),
    }
}

#[test]
fn pushed_checkpoint_with_real_marker_resumes_at_pr_created() {
    let conn = in_memory_conn();
    let run_id = "run-pushed-marker";

    checkpoint_with_marker(&conn, run_id, FinalizationStage::Pushed, Some("def456"));

    match resume_from_checkpoint(&conn, run_id).expect("resume") {
        ResumeAction::Skip(FinalizationStage::PrCreated) => {}
        other => panic!("expected Skip(PrCreated), got {other:?}"),
    }
}

#[test]
fn pr_created_checkpoint_with_real_marker_resumes_at_commented() {
    let conn = in_memory_conn();
    let run_id = "run-pr-created-marker";

    checkpoint_with_marker(&conn, run_id, FinalizationStage::PrCreated, Some("42"));

    match resume_from_checkpoint(&conn, run_id).expect("resume") {
        ResumeAction::Skip(FinalizationStage::Commented) => {}
        other => panic!("expected Skip(Commented), got {other:?}"),
    }
}

#[test]
fn commented_checkpoint_with_real_marker_resumes_at_awaiting_review() {
    let conn = in_memory_conn();
    let run_id = "run-commented-marker";

    checkpoint_with_marker(&conn, run_id, FinalizationStage::Commented, Some("98765"));

    match resume_from_checkpoint(&conn, run_id).expect("resume") {
        ResumeAction::Skip(FinalizationStage::AwaitingReview) => {}
        other => panic!("expected Skip(AwaitingReview), got {other:?}"),
    }
}

#[test]
fn awaiting_review_checkpoint_resumes_done() {
    let conn = in_memory_conn();
    let run_id = "run-awaiting-review";

    checkpoint_with_marker(&conn, run_id, FinalizationStage::AwaitingReview, None);

    // AwaitingReview is the last non-terminal stage; the runtime
    // advances to Done (terminal). The poller picks up the
    // terminal transition from there.
    match resume_from_checkpoint(&conn, run_id).expect("resume") {
        ResumeAction::Skip(FinalizationStage::Done) => {}
        other => panic!("expected Skip(Done), got {other:?}"),
    }
}

#[test]
fn done_checkpoint_returns_already_done() {
    let conn = in_memory_conn();
    let run_id = "run-done";

    checkpoint_with_marker(&conn, run_id, FinalizationStage::Done, None);

    match resume_from_checkpoint(&conn, run_id).expect("resume") {
        ResumeAction::AlreadyDone => {}
        other => panic!("expected AlreadyDone, got {other:?}"),
    }
}
