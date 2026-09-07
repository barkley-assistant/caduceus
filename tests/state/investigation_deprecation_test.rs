//! Investigation deprecation tests (issue #329, DAR §12, §13, §15).
//!
//! Release N keeps Investigation fully active: new admissions are
//! accepted with a deprecation warning, in-flight entries drain under
//! the legacy path, and the migration chain stays structural-only.
//! This binary pins those properties against the FROZEN pre-N fixture
//! (REAL v7-era state, commit `a89c5f2`, see its ORIGIN.md):
//!
//! - AC1 + AC4: `enqueue_summaries` admits a new Investigation entry
//!   AND emits `investigation_admitted_deprecated` — admission is not
//!   rejected (that is #331, release N+1).
//! - AC3 (drain half; the load half is #327's
//!   `review_fixture_migration_test`): the fixture's investigation
//!   entry (owner/r#23) drains to `Done` through the real claim →
//!   `complete_investigation` runtime path on BOTH backends, the
//!   `investigation_archived` audit event fires, and the sibling code
//!   entry (owner/r#17) is untouched — nothing silently dropped.
//! - AC5: re-opening the migrated SQLite store is a no-op (schema
//!   version and row count unchanged) — the migration chain is
//!   idempotent.
//!
//! Fixtures are embedded at compile time and staged into a temp dir
//! before loading, so the tests are hermetic and never mutate the
//! committed artefacts. Tracing captures use the
//! `serial_test::serial` + `tracing_appender::non_blocking` discipline
//! (`tracing_core` caches callsite interest process-wide, the #167
//! finding).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use caduceus::daemon::tick::awaiting_review::{
    enqueue_summaries, INVESTIGATION_DEPRECATED_ADMISSION_EVENT,
};
use caduceus::issue::IssueKey;
use caduceus::logging::build_test_subscriber;
use caduceus::orchestration::ActiveRunGuard;
use caduceus::poll::IssueSummary;
use caduceus::queue::{Phase, StateStore, TicketType};
use caduceus::state::store::{open, DB_FILENAME, SCHEMA_VERSION};
use chrono::Utc;
use rusqlite::Connection;

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

// ---------------------------------------------------------------------------
// Temp-dir helpers (mirror review_fixture_migration_test.rs)
// ---------------------------------------------------------------------------

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

/// Copy an embedded fixture into `dir` under `filename` and return the
/// path. The committed artefact is never opened in place, so a test
/// crash cannot mutate it.
fn stage_fixture(dir: &Path, filename: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(filename);
    std::fs::write(&path, bytes).expect("stage fixture");
    path
}

/// Stage the JSON pre-N fixture as `state.json` in `dir` and open a
/// JSON-backed store on it.
fn open_json_store_on_pre_n(dir: &Path) -> StateStore {
    stage_fixture(dir, "state.json", PRE_N_JSON.as_bytes());
    StateStore::open(dir).expect("open JSON store on staged pre-N fixture")
}

/// Stage the SQLite pre-N fixture as `state.db` in `dir` and open a
/// SQLite-backed store on it (runs the v7 → v8 → v9 chain).
fn open_sqlite_store_on_pre_n(dir: &Path) -> StateStore {
    stage_fixture(dir, DB_FILENAME, PRE_N_DB);
    StateStore::open_sqlite(dir).expect("open SQLite store on staged pre-N fixture")
}

// ---------------------------------------------------------------------------
// Frozen pre-N fixture (REAL v7-era state, commit a89c5f2)
// ---------------------------------------------------------------------------

const PRE_N_DB: &[u8] = include_bytes!("../fixtures/state/pre-n/state.db");
const PRE_N_JSON: &str = include_str!("../fixtures/state/pre-n/state.json");

const CODE_KEY: &str = "owner/r#17";
const INVESTIGATION_KEY: &str = "owner/r#23";

fn code_issue_key() -> IssueKey {
    IssueKey::parse("owner/r#17").expect("parse owner/r#17")
}

fn investigation_issue_key() -> IssueKey {
    IssueKey::parse("owner/r#23").expect("parse owner/r#23")
}

/// The staged fixture must really carry the two queued entries the
/// drain tests consume (guards against a silent fixture swap).
fn assert_fixture_shape(store: &StateStore) {
    let snap = store.snapshot().expect("snapshot");
    assert_eq!(snap.entries.len(), 2, "fixture has exactly two entries");
    let code = snap
        .entry(&code_issue_key())
        .unwrap_or_else(|| panic!("{CODE_KEY} missing"));
    assert_eq!(code.phase, Phase::Queued);
    assert_eq!(code.ticket_type, TicketType::Code);
    let investigation = snap
        .entry(&investigation_issue_key())
        .unwrap_or_else(|| panic!("{INVESTIGATION_KEY} missing"));
    assert_eq!(investigation.phase, Phase::Queued);
    assert_eq!(investigation.ticket_type, TicketType::Investigation);
}

// ---------------------------------------------------------------------------
// Task D (AC3) — pre-N → N drain regression, both backends
// ---------------------------------------------------------------------------

/// Shared drain body: load the fixture, claim each entry through the
/// real `acquire_next` path, and drive them to their terminals
/// through the legacy drain path (`ActiveRunGuard::finish_success` /
/// `finish_investigation` — the exact boundary that emits the
/// archived-outcome audit event). Asserts both entries reach `Done`
/// and the `investigation_archived` event fired while nothing was
/// silently dropped.
fn drain_investigation_to_done(store: Arc<StateStore>, capture: &Path) {
    assert_fixture_shape(&store);

    // Capture the investigation_archived audit event around the drain
    // (serial discipline: callsite interest is cached process-wide).
    // The runtime is built inside `with_default` so the traced
    // `finish_*` calls run under the capture subscriber.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(capture)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            // The real claim path picks the FIFO head — owner/r#17
            // (code) queued first, so both entries drain in FIFO
            // order: code via `finish_success`, investigation via
            // `finish_investigation`. Nothing is silently dropped;
            // each entry ends at its expected terminal.
            let now = Utc::now();
            let first = store
                .acquire_next("RUN-DRAIN-1", std::process::id(), now)
                .expect("acquire first")
                .expect("first entry claimable");
            assert_eq!(first.entry.ticket_type, TicketType::Code);
            let code_key = first.entry.key.clone();
            let mut code_guard = ActiveRunGuard::new(
                first.claim,
                store.clone(),
                PathBuf::from("/dev/null"),
                code_key,
            );
            code_guard
                .finish_success()
                .await
                .expect("code entry drains to Done");

            let second = store
                .acquire_next("RUN-DRAIN-2", std::process::id(), now)
                .expect("acquire second")
                .expect("second entry claimable");
            assert_eq!(second.entry.ticket_type, TicketType::Investigation);
            let inv_key = second.entry.key.clone();
            let mut inv_guard = ActiveRunGuard::new(
                second.claim,
                store.clone(),
                PathBuf::from("/dev/null"),
                inv_key,
            );
            inv_guard
                .finish_investigation()
                .await
                .expect("investigation entry drains to Done");
        });
    });
    drop(guard); // flush pending events + shut down the writer thread

    // Nothing silently dropped: both entries reached Done, code first.
    let snap = store.snapshot().expect("snapshot after drain");
    assert_eq!(snap.entries.len(), 2, "no entry dropped during drain");
    let code = snap
        .entry(&code_issue_key())
        .unwrap_or_else(|| panic!("{CODE_KEY} missing after drain"));
    assert_eq!(code.phase, Phase::Done, "{CODE_KEY} drained to Done");
    let investigation = snap
        .entry(&investigation_issue_key())
        .unwrap_or_else(|| panic!("{INVESTIGATION_KEY} missing after drain"));
    assert_eq!(
        investigation.phase,
        Phase::Done,
        "{INVESTIGATION_KEY} drained to Done"
    );

    // The archived-outcome audit event fired from the drain path.
    let body = std::fs::read_to_string(capture).expect("read capture file");
    assert!(
        body.contains("\"event\":\"investigation_archived\""),
        "investigation_archived audit event missing: {body}"
    );
    assert!(
        body.contains("\"source\":\"drain/finish_investigation\""),
        "drain source literal missing: {body}"
    );
    assert!(
        body.contains("\"repo\":\"r\""),
        "repo field missing: {body}"
    );
}

/// AC3 (issue #329): the frozen pre-N JSON fixture's in-flight
/// investigation entry drains to `Done` through the normal runtime
/// path; the audit writer archives the outcome; nothing silently
/// dropped.
#[test]
fn pre_n_json_investigation_drains_to_done() {
    let dir = tempdir("inv-deprec-json");
    let store = Arc::new(open_json_store_on_pre_n(&dir));
    drain_investigation_to_done(store, &dir.join("drain.log"));
}

/// AC3 (issue #329): same drain contract on the SQLite backend (the
/// fixture migrates v7 → v8 → v9 on open).
#[test]
fn pre_n_sqlite_investigation_drains_to_done() {
    let dir = tempdir("inv-deprec-sqlite");
    let store = Arc::new(open_sqlite_store_on_pre_n(&dir));
    drain_investigation_to_done(store, &dir.join("drain.log"));
}

// ---------------------------------------------------------------------------
// Task E (AC5) — migration idempotency
// ---------------------------------------------------------------------------

/// AC5 (issue #329): re-opening an already-migrated pre-N SQLite store
/// is a no-op — schema version and row count unchanged, no
/// double-migration, no row mutation.
#[test]
fn sqlite_migration_chain_is_idempotent() {
    let dir = tempdir("inv-deprec-idem");

    // First open: v7 → v8 → v9.
    {
        let store = open_sqlite_store_on_pre_n(&dir);
        assert_fixture_shape(&store);
    }

    let read_state = |path: &Path| -> (i64, i64) {
        let conn = Connection::open(path).expect("open migrated db");
        let version = schema_version_of(&conn);
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM queue_entries", [], |r| r.get(0))
            .expect("count queue_entries");
        (version, rows)
    };

    let db_path = dir.join(DB_FILENAME);
    let (version_first, rows_first) = read_state(&db_path);
    assert_eq!(
        version_first, SCHEMA_VERSION,
        "first open must land on the current schema version"
    );
    assert_eq!(rows_first, 2, "both fixture rows survive the migration");

    // Second open on the SAME file: no double-migration, no row churn.
    {
        let conn = open(&db_path).expect("re-open migrated store");
        drop(conn);
    }
    let (version_second, rows_second) = read_state(&db_path);
    assert_eq!(
        version_second, version_first,
        "schema version must not change on re-open"
    );
    assert_eq!(
        rows_second, rows_first,
        "row count must not change on re-open"
    );

    // The review-era tables materialised once and stay structural.
    let conn = Connection::open(&db_path).expect("reopen for table probe");
    for review_table in ["review_queue_entries", "review_state", "review_history"] {
        assert!(
            table_exists(&conn, review_table),
            "review-era table {review_table} must exist after re-open"
        );
    }
}

// ---------------------------------------------------------------------------
// Task F (AC1 + AC4) — admission warning
// ---------------------------------------------------------------------------

/// AC1 + AC4 (issue #329): a new Investigation admission emits
/// `investigation_admitted_deprecated` AND is admitted to `Queued` —
/// release N warns but does not reject (rejection is #331, N+1).
#[test]
#[serial_test::serial]
fn investigation_admission_emits_deprecation_warning_and_admits() {
    let dir = tempdir("inv-deprec-admit");
    let store = StateStore::open(&dir).expect("open fresh store");

    let summary = IssueSummary {
        key: IssueKey::parse("owner/r#31").expect("parse owner/r#31"),
        title: "Investigate flaky loop".to_string(),
        labels: vec!["autofix-investigate".to_string()],
        ticket_type: TicketType::Investigation,
        updated_at: Utc::now(),
    };

    let log_path = dir.join("admission.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    let earliest = tracing::subscriber::with_default(subscriber, || {
        enqueue_summaries(&store, &[summary], false).expect("enqueue_summaries")
    });
    drop(guard);

    // AC4: the entry WAS admitted — phase Queued, not rejected/dropped.
    let snap = store.snapshot().expect("snapshot after admission");
    let key = IssueKey::parse("owner/r#31").expect("reparse key");
    let entry = snap
        .entry(&key)
        .expect("investigation entry must be admitted, not rejected");
    assert_eq!(entry.phase, Phase::Queued);
    assert_eq!(entry.ticket_type, TicketType::Investigation);

    // AC1: the deprecation warning carried the structured event.
    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!(
            "\"event\":\"{INVESTIGATION_DEPRECATED_ADMISSION_EVENT}\""
        )),
        "admission deprecation event missing: {body}"
    );
    assert!(
        body.contains("\"repo\":\"r\""),
        "repo field missing: {body}"
    );
    assert!(body.contains("\"issue\":31"), "issue field missing: {body}");
    // The backoff bookkeeping is unchanged: no next_attempt_at on a
    // fresh insert, so `earliest` stays None.
    assert!(earliest.is_none(), "fresh insert carries no backoff");

    // The code-type counterpart must NOT warn: a code admission on the
    // same store emits no deprecation event (AC1 scope is
    // investigation-only).
    let code_summary = IssueSummary {
        key: IssueKey::parse("owner/r#32").expect("parse owner/r#32"),
        title: "Fix the loop".to_string(),
        labels: vec!["autofix".to_string()],
        ticket_type: TicketType::Code,
        updated_at: Utc::now(),
    };
    let log_path2 = dir.join("admission-code.log");
    let file2 = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path2)
        .expect("open second capture file");
    let (writer2, guard2) = tracing_appender::non_blocking(file2);
    let subscriber2 = build_test_subscriber(writer2);
    tracing::subscriber::with_default(subscriber2, || {
        enqueue_summaries(&store, &[code_summary], false).expect("enqueue code summary")
    });
    drop(guard2);
    let body2 = std::fs::read_to_string(&log_path2).expect("read second capture");
    assert!(
        !body2.contains(INVESTIGATION_DEPRECATED_ADMISSION_EVENT),
        "code admission must not emit the investigation deprecation event: {body2}"
    );

    // Re-admitting the same investigation (AlreadyPresent) must not
    // re-warn — the warning keys on `Inserted` only (no log spam).
    let log_path3 = dir.join("admission-repeat.log");
    let file3 = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path3)
        .expect("open third capture file");
    let (writer3, guard3) = tracing_appender::non_blocking(file3);
    let subscriber3 = build_test_subscriber(writer3);
    tracing::subscriber::with_default(subscriber3, || {
        enqueue_summaries(
            &store,
            &[IssueSummary {
                key: IssueKey::parse("owner/r#31").expect("reparse key"),
                title: "Investigate flaky loop".to_string(),
                labels: vec!["autofix-investigate".to_string()],
                ticket_type: TicketType::Investigation,
                updated_at: Utc::now(),
            }],
            false,
        )
        .expect("re-enqueue investigation summary");
    });
    drop(guard3);
    let body3 = std::fs::read_to_string(&log_path3).expect("read third capture");
    assert!(
        !body3.contains(INVESTIGATION_DEPRECATED_ADMISSION_EVENT),
        "AlreadyPresent must not re-warn: {body3}"
    );
}

// ---------------------------------------------------------------------------
// Task C (AC2) — audit seam is the shared code point
// ---------------------------------------------------------------------------

/// AC2 (issue #329): the audit seam emits the exact structured event
/// the N+1 terminate path (#331) will reuse — same function, same
/// event, `source` distinguishes the callers.
#[test]
#[serial_test::serial]
fn audit_seam_emits_investigation_archived_with_source_field() {
    use caduceus::runtime::audit::emit_investigation_archived;

    let dir = tempdir("inv-deprec-audit");
    let log_path = dir.join("audit.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        emit_investigation_archived("owner", 23, "drain/finish_investigation", "done");
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains("\"event\":\"investigation_archived\""),
        "event name missing: {body}"
    );
    assert!(
        body.contains("\"repo\":\"owner\""),
        "repo field missing: {body}"
    );
    assert!(body.contains("\"issue\":23"), "issue field missing: {body}");
    assert!(
        body.contains("\"source\":\"drain/finish_investigation\""),
        "source field missing: {body}"
    );
    assert!(
        body.contains("\"outcome\":\"done\""),
        "outcome field missing: {body}"
    );
}
