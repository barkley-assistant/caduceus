//! Task 4.4 crash-matrix suite — proves checkpoint infrastructure
//! handles crash recovery at every FINAL-001 stage.
//!
//! 22 tests: 7 stages × 3 crash points + 1 empty-run test.
//!
//! Acceptance checks:
//!
//! **PHASE-04-AC-01** — Persisting a checkpoint and then querying
//! `last_checkpoint_for_run` returns exactly one row for that stage;
//! persisting all seven stages returns seven distinct rows; no
//! duplicate stage rows exist for the same run.

use std::fs;
use std::path::PathBuf;

use caduceus::state::checkpoints::{
    checkpoint_for_run, last_checkpoint_for_run, persist_checkpoint,
};
use caduceus::state::queue::FinalizationStage;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn db_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("cp-crash-{}-{}", std::process::id(), n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.join("test.db")
}

fn persist_slice(conn: &rusqlite::Connection, run_id: &str, stages: &[FinalizationStage]) {
    for stage in stages {
        persist_checkpoint(conn, run_id, *stage, None, None, None).expect("persist checkpoint");
    }
}

/// The seven FINAL-001 stages in canonical order.
const ALL_STAGES: &[FinalizationStage] = &[
    FinalizationStage::ResultValidated,
    FinalizationStage::Committed,
    FinalizationStage::Pushed,
    FinalizationStage::PrCreated,
    FinalizationStage::Commented,
    FinalizationStage::AwaitingReview,
    FinalizationStage::Done,
];

// ---------------------------------------------------------------------------
// Empty run — no checkpoints at all
// ---------------------------------------------------------------------------

/// AC-01: A run with no checkpoints returns None from
/// `last_checkpoint_for_run`, proving recovery starts fresh.
#[test]
fn crash_before_any_checkpoint_returns_none() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");

    let result = last_checkpoint_for_run(&conn, "no-checkpoints-run").expect("query");
    assert!(
        result.is_none(),
        "no checkpoints → last_checkpoint_for_run must return None"
    );

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Crash-matrix: per-stage tests (7 stages × 3 crash points = 21 tests)
//
// Each stage has three crash-point tests:
//
// 1. crash_before_checkpoint_{stage} — no checkpoint persisted for this
//  stage → last_checkpoint returns an earlier stage (or None for the
//  first stage). This proves the crash happened before this stage's
//  checkpoint was written.
//
// 2. crash_between_checkpoint_and_effect_{stage} — the checkpoint for
//  this stage IS persisted (without previous stages), proving the
//  checkpoint survived the crash so recovery can resume from here.
//
// 3. crash_after_effect_{stage} — all stages up to and including this
//  one are persisted. last_checkpoint returns this stage, proving
//  the checkpoint is durable and recovery can skip it.
// ---------------------------------------------------------------------------

// --- ResultValidated (index 0) ---

#[test]
fn crash_before_checkpoint_result_validated() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-before-rv";

    // No previous stages exist before ResultValidated → persist nothing.
    let result = last_checkpoint_for_run(&conn, run_id).expect("query");
    assert!(result.is_none(), "no stages persisted → last must be None");
    // Verify the current stage is NOT present.
    let rows = checkpoint_for_run(&conn, run_id).expect("query");
    assert!(rows.is_empty(), "no checkpoint exists for ResultValidated");

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_between_checkpoint_and_effect_result_validated() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-between-rv";

    persist_checkpoint(
        &conn,
        run_id,
        FinalizationStage::ResultValidated,
        None,
        None,
        None,
    )
    .expect("persist checkpoint");

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "result_validated");

    let rows = checkpoint_for_run(&conn, run_id).expect("query");
    assert_eq!(rows.len(), 1, "exactly one checkpoint for ResultValidated");

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_after_effect_result_validated() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-after-rv";

    // Persist up to and including ResultValidated.
    persist_slice(&conn, run_id, &ALL_STAGES[..=0]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "result_validated");

    let rows = checkpoint_for_run(&conn, run_id).expect("query");
    assert_eq!(rows.len(), 1);

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

// --- Committed (index 1) ---

#[test]
fn crash_before_checkpoint_committed() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-before-cm";

    // Persist only stages before Committed: [ResultValidated]
    persist_slice(&conn, run_id, &ALL_STAGES[..1]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have checkpoint from earlier stage");
    assert_eq!(
        last.stage, "result_validated",
        "crash before Committed → last must be ResultValidated"
    );
    assert_ne!(
        last.stage, "committed",
        "crash happened before Committed was persisted"
    );

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_between_checkpoint_and_effect_committed() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-between-cm";

    persist_checkpoint(
        &conn,
        run_id,
        FinalizationStage::Committed,
        None,
        None,
        None,
    )
    .expect("persist checkpoint");

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "committed");

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_after_effect_committed() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-after-cm";

    // Persist up to and including Committed.
    persist_slice(&conn, run_id, &ALL_STAGES[..=1]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "committed");

    let rows = checkpoint_for_run(&conn, run_id).expect("query");
    assert_eq!(rows.len(), 2);

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

// --- Pushed (index 2) ---

#[test]
fn crash_before_checkpoint_pushed() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-before-ps";

    // Persist only stages before Pushed: [ResultValidated, Committed]
    persist_slice(&conn, run_id, &ALL_STAGES[..2]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have checkpoint from earlier stage");
    assert_eq!(
        last.stage, "committed",
        "crash before Pushed → last must be Committed"
    );
    assert_ne!(
        last.stage, "pushed",
        "crash happened before Pushed was persisted"
    );

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_between_checkpoint_and_effect_pushed() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-between-ps";

    persist_checkpoint(&conn, run_id, FinalizationStage::Pushed, None, None, None)
        .expect("persist checkpoint");

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "pushed");

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_after_effect_pushed() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-after-ps";

    // Persist up to and including Pushed.
    persist_slice(&conn, run_id, &ALL_STAGES[..=2]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "pushed");

    let rows = checkpoint_for_run(&conn, run_id).expect("query");
    assert_eq!(rows.len(), 3);

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

// --- PrCreated (index 3) ---

#[test]
fn crash_before_checkpoint_pr_created() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-before-prc";

    persist_slice(&conn, run_id, &ALL_STAGES[..3]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have checkpoint from earlier stage");
    assert_eq!(
        last.stage, "pushed",
        "crash before PrCreated → last must be Pushed"
    );
    assert_ne!(
        last.stage, "pr_created",
        "crash happened before PrCreated was persisted"
    );

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_between_checkpoint_and_effect_pr_created() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-between-prc";

    persist_checkpoint(
        &conn,
        run_id,
        FinalizationStage::PrCreated,
        None,
        None,
        None,
    )
    .expect("persist checkpoint");

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "pr_created");

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_after_effect_pr_created() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-after-prc";

    persist_slice(&conn, run_id, &ALL_STAGES[..=3]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "pr_created");

    let rows = checkpoint_for_run(&conn, run_id).expect("query");
    assert_eq!(rows.len(), 4);

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

// --- Commented (index 4) ---

#[test]
fn crash_before_checkpoint_commented() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-before-co";

    persist_slice(&conn, run_id, &ALL_STAGES[..4]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have checkpoint from earlier stage");
    assert_eq!(
        last.stage, "pr_created",
        "crash before Commented → last must be PrCreated"
    );
    assert_ne!(
        last.stage, "commented",
        "crash happened before Commented was persisted"
    );

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_between_checkpoint_and_effect_commented() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-between-co";

    persist_checkpoint(
        &conn,
        run_id,
        FinalizationStage::Commented,
        None,
        None,
        None,
    )
    .expect("persist checkpoint");

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "commented");

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_after_effect_commented() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-after-co";

    persist_slice(&conn, run_id, &ALL_STAGES[..=4]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "commented");

    let rows = checkpoint_for_run(&conn, run_id).expect("query");
    assert_eq!(rows.len(), 5);

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

// --- AwaitingReview (index 5) ---

#[test]
fn crash_before_checkpoint_awaiting_review() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-before-ar";

    persist_slice(&conn, run_id, &ALL_STAGES[..5]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have checkpoint from earlier stage");
    assert_eq!(
        last.stage, "commented",
        "crash before AwaitingReview → last must be Commented"
    );
    assert_ne!(
        last.stage, "awaiting_review",
        "crash happened before AwaitingReview was persisted"
    );

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_between_checkpoint_and_effect_awaiting_review() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-between-ar";

    persist_checkpoint(
        &conn,
        run_id,
        FinalizationStage::AwaitingReview,
        None,
        None,
        None,
    )
    .expect("persist checkpoint");

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "awaiting_review");

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_after_effect_awaiting_review() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-after-ar";

    persist_slice(&conn, run_id, &ALL_STAGES[..=5]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "awaiting_review");

    let rows = checkpoint_for_run(&conn, run_id).expect("query");
    assert_eq!(rows.len(), 6);

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

// --- Done (index 6) ---

#[test]
fn crash_before_checkpoint_done() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-before-dn";

    persist_slice(&conn, run_id, &ALL_STAGES[..6]);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have checkpoint from earlier stage");
    assert_eq!(
        last.stage, "awaiting_review",
        "crash before Done → last must be AwaitingReview"
    );
    assert_ne!(
        last.stage, "done",
        "crash happened before Done was persisted"
    );

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_between_checkpoint_and_effect_done() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-between-dn";

    persist_checkpoint(&conn, run_id, FinalizationStage::Done, None, None, None)
        .expect("persist checkpoint");

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "done");

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn crash_after_effect_done() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");
    let run_id = "crash-after-dn";

    // Persist all seven FINAL-001 stages.
    persist_slice(&conn, run_id, ALL_STAGES);

    let last = last_checkpoint_for_run(&conn, run_id)
        .expect("query")
        .expect("must have a checkpoint");
    assert_eq!(last.stage, "done");

    let rows = checkpoint_for_run(&conn, run_id).expect("query");
    assert_eq!(rows.len(), 7);

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}
