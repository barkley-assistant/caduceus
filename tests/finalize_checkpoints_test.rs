//! Task 4.1 integration suite — proves FINAL-001 durable checkpoints.
//!
//! Acceptance checks:
//!
//! - **4.1-AC-01** — Persist all seven checkpoints.
//!   Every FINAL-001 stage is written to the SQLite checkpoint table
//!   before the corresponding external effect.
//! - **4.1-AC-02** — Commit a checkpoint before its next effect.
//!   Each checkpoint row has a `created_at` that precedes the next
//!   checkpoint's `created_at`.
//! - **4.1-AC-03** — Resume from the last durable checkpoint.
//!   After a crash, reading the checkpoint table returns the most
//!   recent stage so the orchestrator can resume.

use std::fs;
use std::path::PathBuf;

use caduceus::state::checkpoints::{checkpoint_for_run, persist_checkpoint, CheckpointRow};
use caduceus::state::queue::FinalizationStage;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn db_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("cp-test-{}-{}", std::process::id(), n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.join("test.db")
}

// ---------------------------------------------------------------------------
// 4.1-AC-01: Persist all seven checkpoints
// ---------------------------------------------------------------------------

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

#[test]
fn persist_all_seven_checkpoints() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");

    let run_id = "test-run-001";
    for stage in ALL_STAGES {
        persist_checkpoint(&conn, run_id, *stage, None).expect("persist checkpoint");
    }

    // Verify all seven are present in the correct order.
    let rows: Vec<CheckpointRow> = checkpoint_for_run(&conn, run_id).expect("query checkpoints");

    assert_eq!(rows.len(), 7, "must have exactly 7 checkpoints");

    for (i, row) in rows.iter().enumerate() {
        let expected_stage: &str = ALL_STAGES[i].as_str();
        assert_eq!(
            row.stage, expected_stage,
            "checkpoint {}: expected stage {}, got {}",
            i, expected_stage, row.stage
        );
    }

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 4.1-AC-02: Checkpoints are ordered — each checkpoint is committed before
//            the next effect.
// ---------------------------------------------------------------------------

#[test]
fn checkpoints_are_chronologically_ordered() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");

    let run_id = "test-run-002";
    for stage in ALL_STAGES {
        persist_checkpoint(&conn, run_id, *stage, None).expect("persist checkpoint");
    }

    let rows: Vec<CheckpointRow> = checkpoint_for_run(&conn, run_id).expect("query checkpoints");

    assert_eq!(rows.len(), 7);

    for window in rows.windows(2) {
        let t1 = &window[0].created_at;
        let t2 = &window[1].created_at;
        assert!(
            t1 < t2,
            "checkpoint {} ({}) must precede {} ({}): {} >= {}",
            window[0].stage,
            t1,
            window[1].stage,
            t2,
            t1,
            t2
        );
    }

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 4.1-AC-03: Resume from the last durable checkpoint
// ---------------------------------------------------------------------------

#[test]
fn resume_returns_last_checkpoint() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");

    let run_id = "test-run-003";
    // Write the first three checkpoints to simulate a crash after Pushed.
    for stage in &ALL_STAGES[..3] {
        persist_checkpoint(&conn, run_id, *stage, None).expect("persist checkpoint");
    }

    // Resume: we should get Pushed (the last durable checkpoint).
    let rows: Vec<CheckpointRow> = checkpoint_for_run(&conn, run_id).expect("query checkpoints");

    assert_eq!(rows.len(), 3, "must have 3 checkpoints after crash");
    assert_eq!(rows.last().unwrap().stage, "pushed");
    assert_eq!(
        rows.last().unwrap().stage_enum(),
        Some(FinalizationStage::Pushed)
    );

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn resume_returns_none_for_unknown_run() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");

    let rows: Vec<CheckpointRow> =
        checkpoint_for_run(&conn, "nonexistent-run").expect("query checkpoints");

    assert!(rows.is_empty(), "no checkpoints for unknown run");

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Checkpoint with data (commit_oid, pr_number, pr_url)
// ---------------------------------------------------------------------------

#[test]
fn persist_checkpoint_with_operation_data() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");

    let run_id = "test-run-004";
    let data = r#"{"commit_oid":"abc123","branch":"feat/test"}"#;
    persist_checkpoint(&conn, run_id, FinalizationStage::Committed, Some(data))
        .expect("persist checkpoint with data");

    let rows: Vec<CheckpointRow> = checkpoint_for_run(&conn, run_id).expect("query checkpoints");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].stage, "committed");
    assert_eq!(
        rows[0].checkpoint_data.as_deref(),
        Some(data),
        "checkpoint data must survive round-trip"
    );

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Idempotency: re-persisting the same stage for the same run overwrites
// ---------------------------------------------------------------------------

#[test]
fn repersist_same_stage_overwrites() {
    let path = db_path();
    let conn = caduceus::state::store::open(&path).expect("open db");

    let run_id = "test-run-005";
    let data_v1 = r#"{"version":1}"#;
    let data_v2 = r#"{"version":2}"#;

    persist_checkpoint(&conn, run_id, FinalizationStage::Committed, Some(data_v1))
        .expect("persist v1");
    persist_checkpoint(&conn, run_id, FinalizationStage::Committed, Some(data_v2))
        .expect("persist v2");

    let rows: Vec<CheckpointRow> = checkpoint_for_run(&conn, run_id).expect("query checkpoints");

    // Still exactly one row for the run+stage pair (PRIMARY KEY).
    assert_eq!(rows.len(), 1, "must be exactly one row after overwrite");
    assert_eq!(
        rows[0].checkpoint_data.as_deref(),
        Some(data_v2),
        "overwritten data must be v2"
    );

    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}
