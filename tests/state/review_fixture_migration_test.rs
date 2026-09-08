//! Release-N fixture migration/load tests (issue #327, DAR §4.4, §15).
//!
//! The committed fixtures under `tests/fixtures/state/` are FROZEN
//! compatibility artefacts: `pre-n/` is a REAL v7-era store (generated
//! by the pre-Auto-Review binary at `a89c5f2`, see its ORIGIN.md) and
//! `n-era/` is the current release-N shape (see its ORIGIN.md).
//!
//! This binary pins the release-N gate criteria: every fixture exists,
//! is structurally valid at its committed envelope version, and
//! loads/migrates cleanly under CURRENT code on both backends (JSON +
//! SQLite). The N+1-binary upgrade and reconciliation tests are #331's
//! — out of scope here (DAR §15 acceptance split).
//!
//! Fixtures are embedded at compile time (`include_bytes!` /
//! `include_str!`) and staged into a temp dir before loading, so the
//! tests are hermetic (no CWD dependence) and never mutate the
//! committed artefacts.

use std::fs;
use std::path::{Path, PathBuf};

use caduceus::queue::{parse_queue_state, StateStore, QUEUE_FILE_VERSION};
use caduceus::review::REVIEW_SCHEMA_VERSION;
use caduceus::state::review::{
    parse_review_history, parse_review_queue_state, parse_review_state_map, ReviewStore,
    REVIEW_HISTORY_FILE_VERSION, REVIEW_QUEUE_FILE_VERSION, REVIEW_STATE_FILE_VERSION,
};
use caduceus::store::{open, sqlite_migration_chain, SCHEMA_VERSION};
use rusqlite::Connection;

// ---------------------------------------------------------------------------
// Temp-dir helper (mirrors review_activation_test.rs)
// ---------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "review-fixture-migration-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |r| r.get(0),
        )
        .expect("probe sqlite_master");
    count > 0
}

fn schema_version_of(conn: &Connection) -> i64 {
    conn.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .expect("read schema_version")
}

/// Copy an embedded SQLite fixture to `<dir>/state.db` and return the
/// path. The committed artefact is never opened in place, so a test
/// crash cannot mutate it.
fn stage_db(dir: &Path, bytes: &[u8]) -> PathBuf {
    let db_path = dir.join("state.db");
    fs::write(&db_path, bytes).expect("stage sqlite fixture");
    db_path
}

// ---------------------------------------------------------------------------
// Pre-N fixtures (REAL v7-era state, commit a89c5f2)
// ---------------------------------------------------------------------------

const PRE_N_DB: &[u8] = include_bytes!("../fixtures/state/pre-n/state.db");
const PRE_N_JSON: &str = include_str!("../fixtures/state/pre-n/state.json");
const PRE_N_ORIGIN: &str = include_str!("../fixtures/state/pre-n/ORIGIN.md");

/// AC1 (issue #327): the frozen pre-N SQLite fixture runs the full
/// v7 → v8 → v9 migration chain through the real `open()`, the
/// review-era tables materialise, and the issue data survives.
#[test]
fn pre_n_sqlite_loads_and_migrates_v7_to_current() {
    let dir = temp_dir("pre-n-sqlite");
    let db_path = stage_db(&dir, PRE_N_DB);

    // Sanity: the committed artefact really is a v7 store (the era
    // marker this fixture exists to pin).
    {
        let conn = Connection::open(&db_path).expect("open committed pre-n db");
        assert_eq!(schema_version_of(&conn), 7, "pre-N fixture must be v7");
        for review_table in ["review_queue_entries", "review_state", "review_history"] {
            assert!(
                !table_exists(&conn, review_table),
                "pre-N fixture must have no review-era table {review_table}"
            );
        }
    }

    // The real store-open path: v7 → v8 → v9 chain + apply_schema.
    {
        let conn = open(&db_path).expect("open() migrates the v7 pre-N store");
        drop(conn);
    }

    let conn = Connection::open(&db_path).expect("reopen migrated db");
    assert_eq!(
        schema_version_of(&conn),
        SCHEMA_VERSION,
        "migration must land on the current schema version"
    );
    for review_table in ["review_queue_entries", "review_state", "review_history"] {
        assert!(
            table_exists(&conn, review_table),
            "review-era table {review_table} must be created by the migration"
        );
    }
    // Issue data survives the v7→v8→v9 walk.
    let queue_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM queue_entries", [], |r| r.get(0))
        .expect("count queue_entries");
    assert!(
        queue_rows >= 1,
        "pre-N issue data must survive the migration"
    );
}

/// The v7 → v9 path this fixture traverses is exactly the registered
/// migration chain (guard against the registry silently shrinking).
#[test]
fn pre_n_migration_chain_covers_v7_to_current() {
    let chain = sqlite_migration_chain();
    assert!(
        chain.iter().any(|(from, to, _)| *from == 7 && *to == 8),
        "chain must contain the v7→v8 review-era activation step"
    );
    assert!(
        chain.iter().any(|(from, to, _)| *from == 8 && *to == 9),
        "chain must contain the v8→v9 blocked-columns step"
    );
    let last_to = chain.last().map(|(_, to, _)| *to);
    assert_eq!(
        last_to,
        Some(SCHEMA_VERSION),
        "chain must end at the current schema version"
    );
}

/// AC1 (issue #327) + N+1 reconcile (issue #331, DAR §4.4): the
/// frozen pre-N JSON fixture is a v1 envelope that current code
/// accept-and-upgrades to the current version. The `parse_queue_state`
/// seam is pure (no file write); the real store-open path now PERSISTS
/// when the N+1 reconcile pass terminates a live investigation row —
/// that rewrite is the reconcile's durable archive, not a silent
/// upgrade.
#[test]
fn pre_n_json_loads_and_upgrades_v1_to_current() {
    let parsed = parse_queue_state(PRE_N_JSON).expect("parse pre-N v1 state.json");
    assert_eq!(
        parsed.version, QUEUE_FILE_VERSION,
        "parse seam upgrades v1 in place to the current envelope version"
    );
    assert!(
        !parsed.entries.is_empty(),
        "pre-N JSON must carry at least one issue entry"
    );

    // The real store-open path also accepts the v1 file.
    let dir = temp_dir("pre-n-json");
    let state_path = dir.join("state.json");
    fs::write(&state_path, PRE_N_JSON).expect("stage pre-N json");
    let store = StateStore::open(&dir).expect("open store on the v1 file");
    let snap = store.snapshot().expect("snapshot");
    assert_eq!(snap.version, QUEUE_FILE_VERSION);
    assert_eq!(snap.entries.len(), parsed.entries.len());
    drop(store);

    // N+1 reconcile is a persisting open for this fixture: the live
    // investigation row (`owner/r#23`) is terminated to `skipped` and
    // archived, so the file is rewritten at the current envelope
    // version. Nothing is silently dropped — the code row survives.
    let on_disk = fs::read_to_string(&state_path).unwrap();
    assert!(
        on_disk.contains(&format!(r#""version":{QUEUE_FILE_VERSION}"#)),
        "reconcile must persist at the current envelope; got: {on_disk}"
    );
    assert!(
        on_disk.contains(r#""ticket_type":"code""#),
        "code row must survive the reconcile rewrite; got: {on_disk}"
    );
    assert!(
        on_disk.contains(r#""ticket_type":"investigation""#)
            && on_disk.contains(r#""phase":"skipped""#),
        "investigation row must be terminated to skipped; got: {on_disk}"
    );
}

/// Provenance guard (plan §7): ORIGIN.md must pin the pre-N commit so
/// the fixture's real-binary lineage cannot be silently lost.
#[test]
fn pre_n_fixture_origin_documented() {
    assert!(
        PRE_N_ORIGIN.contains("a89c5f2"),
        "pre-N ORIGIN.md must name the generating commit a89c5f2"
    );
    assert!(
        PRE_N_ORIGIN.contains("v7-era"),
        "pre-N ORIGIN.md must state the v7 era"
    );
}

// ---------------------------------------------------------------------------
// N-era fixtures (current release-N shape)
// ---------------------------------------------------------------------------

const N_ERA_DB: &[u8] = include_bytes!("../fixtures/state/n-era/state.db");
const N_ERA_JSON: &str = include_str!("../fixtures/state/n-era/state.json");
const N_ERA_REVIEW_QUEUE: &str = include_str!("../fixtures/state/n-era/review_queue.json");
const N_ERA_REVIEW_STATE: &str = include_str!("../fixtures/state/n-era/review_state.json");
const N_ERA_REVIEW_HISTORY: &str = include_str!("../fixtures/state/n-era/review_history.json");

/// AC2 (issue #327): the N-era SQLite fixture is already at the
/// current schema version, carries the review tables with real review
/// data, and opens cleanly (no migration) — the compatibility artefact
/// #331's reconciliation tests consume.
#[test]
fn n_era_sqlite_loads_cleanly_at_current_version() {
    let dir = temp_dir("n-era-sqlite");
    let db_path = stage_db(&dir, N_ERA_DB);

    // Sanity: the artefact is at the CURRENT version (not pre-migration).
    {
        let conn = Connection::open(&db_path).expect("open committed n-era db");
        assert_eq!(
            schema_version_of(&conn),
            SCHEMA_VERSION,
            "n-era fixture must be at the current schema version"
        );
    }

    // The real store-open path is a no-op migration + validated load.
    let review = ReviewStore::open_sqlite(&dir).expect("open n-era sqlite review store");

    let queue = review.review_queue_snapshot().expect("review queue load");
    assert_eq!(
        queue.version, REVIEW_QUEUE_FILE_VERSION,
        "n-era review queue must be at the current envelope version"
    );
    assert_eq!(
        queue.entries.len(),
        1,
        "n-era fixture must carry exactly one review queue entry"
    );

    // The committed db itself has ≥1 review_state row (the #331
    // reconciliation input).
    let conn = Connection::open(&db_path).expect("reopen n-era db");
    for review_table in ["review_queue_entries", "review_state", "review_history"] {
        assert!(
            table_exists(&conn, review_table),
            "n-era fixture must contain the review-era table {review_table}"
        );
    }
    let state_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM review_state", [], |r| r.get(0))
        .expect("count review_state");
    assert!(
        state_rows >= 1,
        "n-era fixture must carry at least one review_state row"
    );
}

/// AC2 (issue #327): the N-era JSON artefacts — the v2 issue envelope
/// and the three v1 review sidecars — parse at their current envelope
/// versions under current code.
#[test]
fn n_era_json_and_sidecars_load_cleanly() {
    let dir = temp_dir("n-era-json");
    fs::write(dir.join("state.json"), N_ERA_JSON).expect("stage n-era state.json");
    fs::write(
        dir.join(caduceus::state::review::REVIEW_QUEUE_FILENAME),
        N_ERA_REVIEW_QUEUE,
    )
    .expect("stage review_queue.json");
    fs::write(
        dir.join(caduceus::state::review::REVIEW_STATE_FILENAME),
        N_ERA_REVIEW_STATE,
    )
    .expect("stage review_state.json");
    fs::write(
        dir.join(caduceus::state::review::REVIEW_HISTORY_FILENAME),
        N_ERA_REVIEW_HISTORY,
    )
    .expect("stage review_history.json");

    // Issue envelope: loads through the real store path at the current
    // version.
    let store = StateStore::open(&dir).expect("open n-era json store");
    let snap = store.snapshot().expect("snapshot");
    assert_eq!(snap.version, QUEUE_FILE_VERSION);
    drop(store);

    // Review sidecars: validated load at open (all three files).
    let review = ReviewStore::open(&dir).expect("open n-era json review store");

    let queue = review.review_queue_snapshot().expect("review queue load");
    assert_eq!(queue.version, REVIEW_QUEUE_FILE_VERSION);
    assert_eq!(queue.entries.len(), 1);

    // The other two sidecars parse directly at their envelope versions.
    let states = parse_review_state_map(N_ERA_REVIEW_STATE).expect("parse review_state.json");
    assert_eq!(states.version, REVIEW_STATE_FILE_VERSION);
    assert_eq!(states.states.len(), 1);

    let history = parse_review_history(N_ERA_REVIEW_HISTORY).expect("parse review_history.json");
    assert_eq!(history.version, REVIEW_HISTORY_FILE_VERSION);
    assert_eq!(history.rows.len(), 1);
}

/// The N-era ReviewResult blob carried in history embeds the daemon's
/// accepted `REVIEW_SCHEMA_VERSION` — the version the worker harness
/// injects (criterion 3's counterpart contract).
#[test]
fn n_era_history_blob_carries_current_result_schema_version() {
    let history = parse_review_history(N_ERA_REVIEW_HISTORY).expect("parse review_history.json");
    let row = history
        .rows
        .first()
        .expect("n-era history must have one row");
    let blob: serde_json::Value =
        serde_json::from_str(&row.result_json).expect("history result_json parses");
    assert_eq!(
        blob["schema_version"],
        serde_json::json!(REVIEW_SCHEMA_VERSION),
        "n-era history blob must carry the daemon's accepted ReviewResult schema version"
    );
    assert_eq!(blob["review"]["verdict"], "pass");
}

/// Provenance guard: the N-era ORIGIN.md must document the generation
/// method and the D3 era-based naming decision (n-era, not v8/v9).
#[test]
fn n_era_fixture_origin_documented() {
    let origin = include_str!("../fixtures/state/n-era/ORIGIN.md");
    assert!(
        origin.contains("generate_n_era"),
        "n-era ORIGIN.md must document the generator"
    );
    assert!(
        origin.contains("n-era"),
        "n-era ORIGIN.md must document the era-based naming decision"
    );
}

/// AC3/AC4 bridge (issue #327): the review queue key shape and the
/// parse seam stay coherent across both committed eras — the pre-N
/// issue entries and the N-era review entries parse through their
/// respective seams without version ambiguity.
#[test]
fn fixture_parse_seams_are_version_explicit() {
    // Pre-N JSON: parse reports the UPGRADED version, not the file's.
    let pre_n = parse_queue_state(PRE_N_JSON).expect("pre-N parse");
    assert_eq!(pre_n.version, QUEUE_FILE_VERSION);

    // N-era review queue: parse reports the current review envelope.
    let queue = parse_review_queue_state(N_ERA_REVIEW_QUEUE).expect("n-era queue parse");
    assert_eq!(queue.version, REVIEW_QUEUE_FILE_VERSION);
    assert_eq!(queue.entries.len(), 1);
}
