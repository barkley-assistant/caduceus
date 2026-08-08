//! Unit tests for the checkpoint persistence functions.

use caduceus::queue::FinalizationStage;
use caduceus::state::checkpoints::{
    checkpoint_for_run, delete_checkpoints_for_run, last_checkpoint_for_run, persist_checkpoint,
    CheckpointRow,
};
use caduceus::state::store;

#[test]
fn persist_and_read_back() {
    let path = std::env::temp_dir().join(format!("cp-unit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    let db = path.join("test.db");
    let conn = store::open(&db).expect("open db");

    let run_id = "unit-run-1";
    persist_checkpoint(
        &conn,
        run_id,
        FinalizationStage::Committed,
        None,
        None,
        None,
    )
    .expect("persist");
    persist_checkpoint(&conn, run_id, FinalizationStage::Pushed, None, None, None)
        .expect("persist");

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
    persist_checkpoint(
        &conn,
        run_id,
        FinalizationStage::Pushed,
        Some(r#"{"v":1}"#),
        None,
        None,
    )
    .expect("persist v1");
    persist_checkpoint(
        &conn,
        run_id,
        FinalizationStage::Pushed,
        Some(r#"{"v":2}"#),
        None,
        None,
    )
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
    persist_checkpoint(
        &conn,
        run_id,
        FinalizationStage::Committed,
        None,
        None,
        None,
    )
    .expect("persist");
    persist_checkpoint(&conn, run_id, FinalizationStage::Pushed, None, None, None)
        .expect("persist");

    delete_checkpoints_for_run(&conn, run_id).expect("delete");

    let rows: Vec<CheckpointRow> = checkpoint_for_run(&conn, run_id).expect("query");
    assert!(rows.is_empty(), "all checkpoints deleted");

    let _ = std::fs::remove_dir_all(&path);
}
