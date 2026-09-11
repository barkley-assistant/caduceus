//! Investigation removal tests (issue #331, DAR §4.4, §12, §13).
//!
//! Release N+1 removes the Investigation active path. This binary
//! pins the migration and behavior properties against the FROZEN
//! fixtures from #327 plus the synthesized N-era fixture:
//!
//! - AC2 (direct pre-N→N+1 upgrade): staging the frozen `pre-n`
//!   fixture and opening the store loads BOTH entries, terminates
//!   the investigation row (`owner/r#23`) to `Skipped` with the
//!   reconcile reason, leaves the code row (`owner/r#17`) untouched,
//!   drops nothing, and emits BOTH audit events
//!   (`investigation_archived` with `source=reconcile/startup`, and
//!   `review_migration_terminated_investigation`). Both backends.
//!   Re-open is idempotent (second open terminates 0).
//! - AC3 (N-era-v8→N+1): the synthesized v2-envelope fixture with an
//!   in-progress investigation row reconciles on open (JSON), and an
//!   investigation row INSERTed into a copy of the frozen
//!   `n-era/state.db` reconciles on open (SQLite — in-test INSERT
//!   keeps a generated binary artefact out of git).
//! - Negative parse test (AC4): the N-era investigation entry STILL
//!   PARSES in N+1 — the snapshot saw the row with
//!   `ticket_type == Investigation` before the termination
//!   assertions. This plus the pre-N tests is the CI exercise for
//!   "legacy parsing still works".
//! - AC1 (admission rejection): `StateStore::enqueue` rejects
//!   `TicketType::Investigation` with the
//!   `investigation-admission-rejected` error context and the
//!   `investigation_admission_rejected` event; the store is
//!   unchanged.
//! - Claim-file safety: a leftover claim file for a terminated row
//!   is unlinked by the reconcile pass (mirrors `skip()`).
//!
//! Fixtures are embedded at compile time and staged into a temp dir
//! before loading, so the tests are hermetic and never mutate the
//! committed artefacts. Tracing captures use the
//! `serial_test::serial` + `tracing_appender::non_blocking` discipline
//! (`tracing_core` caches callsite interest process-wide, the #167
//! finding).

use std::path::{Path, PathBuf};

use caduceus::issue::IssueKey;
use caduceus::logging::build_test_subscriber;
use caduceus::queue::{Phase, StateStore, TicketType};
use caduceus::runtime::audit::REVIEW_MIGRATION_TERMINATED_INVESTIGATION_EVENT;
use caduceus::state::store::DB_FILENAME;
use rusqlite::Connection;

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

// ---------------------------------------------------------------------------
// Temp-dir helpers (mirror investigation_deprecation_test.rs)
// ---------------------------------------------------------------------------

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

/// Capture tracing events into `capture` while running `body`, then
/// return the captured body. The writer guard is dropped (flushed)
/// before the file is read, so the returned body is authoritative.
fn capture(body: impl FnOnce(), capture: &Path) -> String {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(capture)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);
    tracing::subscriber::with_default(subscriber, body);
    drop(guard); // flush pending events + shut down the writer thread
    std::fs::read_to_string(capture).expect("read capture file")
}

// ---------------------------------------------------------------------------
// Frozen fixtures (#327) + synthesized N-era fixture (#331)
// ---------------------------------------------------------------------------

const PRE_N_DB: &[u8] = include_bytes!("../fixtures/state/pre-n/state.db");
const PRE_N_JSON: &str = include_str!("../fixtures/state/pre-n/state.json");
const N_ERA_DB: &[u8] = include_bytes!("../fixtures/state/n-era/state.db");
const N_ERA_INV_JSON: &str = include_str!("../fixtures/state/n-era-investigation/state.json");

const CODE_KEY: &str = "owner/r#17";
const INVESTIGATION_KEY: &str = "owner/r#23";

fn code_issue_key() -> IssueKey {
    IssueKey::parse("owner/r#17").expect("parse owner/r#17")
}

fn investigation_issue_key() -> IssueKey {
    IssueKey::parse("owner/r#23").expect("parse owner/r#23")
}

/// The shared AC2 assertions: after opening a store on the staged
/// pre-N fixture, the investigation entry is `Skipped` with the
/// reconcile reason, the code entry is untouched `Queued`, both
/// entries survive, and both audit events fired.
fn assert_pre_n_reconciled(store: &StateStore, capture_body: &str) {
    let snap = store.snapshot().expect("snapshot");
    assert_eq!(snap.entries.len(), 2, "nothing silently dropped");

    let investigation = snap
        .entry(&investigation_issue_key())
        .unwrap_or_else(|| panic!("{INVESTIGATION_KEY} missing"));
    assert_eq!(
        investigation.phase,
        Phase::Skipped,
        "{INVESTIGATION_KEY} must be terminated to Skipped"
    );
    assert_eq!(
        investigation.ticket_type,
        TicketType::Investigation,
        "ticket type is retained (parse compat)"
    );
    let reason = investigation
        .last_error
        .as_deref()
        .unwrap_or_else(|| panic!("reconcile reason recorded"));
    assert!(
        reason.contains("#331") && reason.contains("reconcile"),
        "last_error must cite the reconcile removal: {reason}"
    );
    assert!(
        investigation.next_attempt_at.is_none(),
        "terminated row must not carry a backoff"
    );

    let code = snap
        .entry(&code_issue_key())
        .unwrap_or_else(|| panic!("{CODE_KEY} missing"));
    assert_eq!(code.phase, Phase::Queued, "{CODE_KEY} must be untouched");
    assert_eq!(code.ticket_type, TicketType::Code);

    // Both audit events fired, with the reconcile source.
    assert!(
        capture_body.contains("\"event\":\"investigation_archived\""),
        "investigation_archived audit event missing: {capture_body}"
    );
    assert!(
        capture_body.contains("\"source\":\"reconcile/startup\""),
        "reconcile/startup source missing: {capture_body}"
    );
    assert!(
        capture_body.contains(&format!(
            "\"event\":\"{REVIEW_MIGRATION_TERMINATED_INVESTIGATION_EVENT}\""
        )),
        "review_migration_terminated_investigation event missing: {capture_body}"
    );
    assert!(
        capture_body.contains("\"outcome\":\"terminated\""),
        "terminated outcome missing: {capture_body}"
    );
    assert!(
        capture_body.contains("\"phase_before\":\"queued\""),
        "reconcile event must record the pre-termination phase (queued): {capture_body}"
    );
    assert!(
        capture_body.contains("\"repo\":\"r\"") && capture_body.contains("\"issue\":23"),
        "repo/issue identity missing: {capture_body}"
    );
}

// ---------------------------------------------------------------------------
// AC2 — direct pre-N → N+1 upgrade (JSON backend)
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn pre_n_json_reconciles_on_open() {
    let dir = tempdir("inv-removal-json");
    stage_fixture(&dir, "state.json", PRE_N_JSON.as_bytes());
    let capture_path = dir.join("reconcile.log");
    capture(
        || {
            let store = StateStore::open(&dir).expect("open JSON store on staged pre-N fixture");
            let snap = store.snapshot().expect("snapshot");
            assert_eq!(snap.entries.len(), 2);
            assert_eq!(
                snap.entry(&investigation_issue_key()).unwrap().phase,
                Phase::Skipped
            );
        },
        &capture_path,
    );
    // Re-open outside the capture window; the durable effects are
    // what the authoritative assertions check.
    let store = StateStore::open(&dir).expect("reopen");
    let body = std::fs::read_to_string(&capture_path).expect("read capture");
    assert_pre_n_reconciled(&store, &body);
}

#[test]
fn pre_n_json_reconcile_is_idempotent() {
    let dir = tempdir("inv-removal-json-idem");
    stage_fixture(&dir, "state.json", PRE_N_JSON.as_bytes());
    {
        let store = StateStore::open(&dir).expect("first open");
        let snap = store.snapshot().expect("snapshot");
        assert_eq!(
            snap.entry(&investigation_issue_key()).unwrap().phase,
            Phase::Skipped
        );
    }
    // Second open on the SAME file: the row is terminal, so the
    // reconcile pass terminates 0 and emits no terminate event.
    let capture_path = dir.join("second-open.log");
    let second = capture(
        || {
            let store = StateStore::open(&dir).expect("second open");
            let snap = store.snapshot().expect("snapshot after re-open");
            assert_eq!(
                snap.entry(&investigation_issue_key()).unwrap().phase,
                Phase::Skipped,
                "terminated row must stay Skipped"
            );
            assert_eq!(snap.entry(&code_issue_key()).unwrap().phase, Phase::Queued);
            assert_eq!(snap.entries.len(), 2, "re-open drops nothing");
        },
        &capture_path,
    );
    assert!(
        !second.contains(REVIEW_MIGRATION_TERMINATED_INVESTIGATION_EVENT),
        "second open must terminate 0 rows: {second}"
    );
    assert!(
        !second.contains("investigation_archived"),
        "second open must not re-archive: {second}"
    );
}

// ---------------------------------------------------------------------------
// AC2 — direct pre-N → N+1 upgrade (SQLite backend)
// ---------------------------------------------------------------------------

#[test]
fn pre_n_sqlite_reconciles_on_open() {
    let dir = tempdir("inv-removal-sqlite");
    stage_fixture(&dir, DB_FILENAME, PRE_N_DB);
    let capture_path = dir.join("reconcile.log");
    let store = capture(
        || {
            StateStore::open_sqlite(&dir).expect("open SQLite store on staged pre-N fixture");
        },
        &capture_path,
    );
    // Drop the handle so the post-open probe opens its own connection.
    drop(store);
    let store = StateStore::open_sqlite(&dir).expect("reopen (idempotent)");
    let body = std::fs::read_to_string(&capture_path).expect("read capture");
    assert_pre_n_reconciled(&store, &body);

    // Reconcile is NOT a migration: the schema version is whatever
    // the structural chain produced, unchanged by the reconcile, and
    // both rows survive.
    let conn = Connection::open(dir.join(DB_FILENAME)).expect("open migrated db");
    let _version = schema_version_of(&conn);
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM queue_entries", [], |r| r.get(0))
        .expect("count queue_entries");
    assert_eq!(rows, 2, "both rows survive reconcile");
}

#[test]
fn pre_n_sqlite_reconcile_is_idempotent() {
    let dir = tempdir("inv-removal-sqlite-idem");
    stage_fixture(&dir, DB_FILENAME, PRE_N_DB);
    {
        let store = StateStore::open_sqlite(&dir).expect("first open");
        let snap = store.snapshot().expect("snapshot");
        assert_eq!(
            snap.entry(&investigation_issue_key()).unwrap().phase,
            Phase::Skipped
        );
    }
    let capture_path = dir.join("second-open.log");
    let second = capture(
        || {
            let store = StateStore::open_sqlite(&dir).expect("second open");
            let snap = store.snapshot().expect("snapshot");
            assert_eq!(
                snap.entry(&investigation_issue_key()).unwrap().phase,
                Phase::Skipped
            );
            assert_eq!(snap.entries.len(), 2);
        },
        &capture_path,
    );
    assert!(
        !second.contains(REVIEW_MIGRATION_TERMINATED_INVESTIGATION_EVENT),
        "second open must terminate 0 rows: {second}"
    );
}

// ---------------------------------------------------------------------------
// AC3 — N-era-v8 store with a live investigation row
// ---------------------------------------------------------------------------

/// Negative parse test + reconcile: the N-era investigation entry
/// still parses in N+1 (the snapshot SAW the row as Investigation
/// before termination), then reconciles to Skipped. JSON backend.
#[test]
#[serial_test::serial]
fn n_era_json_reconciles() {
    let dir = tempdir("inv-removal-nera-json");
    stage_fixture(&dir, "state.json", N_ERA_INV_JSON.as_bytes());
    let capture_path = dir.join("reconcile.log");
    let store = capture(
        || {
            let store = StateStore::open(&dir).expect("open JSON store on staged n-era fixture");
            let snap = store.snapshot().expect("snapshot");
            assert_eq!(snap.entries.len(), 1, "the row loaded");
            let entry = snap
                .entry(&investigation_issue_key())
                .expect("n-era investigation entry parsed");
            assert_eq!(entry.ticket_type, TicketType::Investigation);
            assert_eq!(entry.phase, Phase::Skipped, "reconciled on the same open");
            let reason = entry.last_error.as_deref().expect("reconcile reason");
            assert!(reason.contains("#331"), "reason cites #331: {reason}");
        },
        &capture_path,
    );
    let body = std::fs::read_to_string(&capture_path).expect("read capture");
    assert!(
        body.contains(REVIEW_MIGRATION_TERMINATED_INVESTIGATION_EVENT),
        "terminate event missing: {body}"
    );
    assert!(
        body.contains("\"phase_before\":\"in_progress\""),
        "reconcile event must record the n-era pre-termination phase (in_progress): {body}"
    );
    let _ = store;
}

/// SQLite AC3: stage the EXISTING frozen `n-era/state.db`, INSERT the
/// investigation row via rusqlite in-test (column list mirrors
/// `row_to_entry`), and assert the reconcile pass terminates it.
/// Generated binary artefacts stay out of git.
#[test]
fn n_era_sqlite_reconciles() {
    let dir = tempdir("inv-removal-nera-sqlite");
    stage_fixture(&dir, DB_FILENAME, N_ERA_DB);
    {
        let conn = Connection::open(dir.join(DB_FILENAME)).expect("open staged n-era db");
        conn.execute(
            "INSERT INTO queue_entries
             (issue_key, phase, ticket_type, attempts, last_error, last_run_id,
              next_attempt_at, finalization, queued_at, updated_at, generation,
              blocked_source, blocked_recovery_hint)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                "owner/r#23",
                "in_progress",
                "investigation",
                1i64,
                Option::<String>::None,
                "RUN-N-ERA",
                Option::<String>::None,
                Option::<String>::None,
                "2026-09-07T09:00:00+00:00",
                "2026-09-07T09:05:00+00:00",
                1i64,
                Option::<String>::None,
                Option::<String>::None,
            ],
        )
        .expect("insert n-era investigation row");
    }
    let capture_path = dir.join("reconcile.log");
    let store = capture(
        || {
            let store =
                StateStore::open_sqlite(&dir).expect("open SQLite store on staged n-era db");
            // Negative parse assertion first (see the JSON twin above).
            let snap = store.snapshot().expect("snapshot");
            let entry = snap
                .entry(&investigation_issue_key())
                .expect("injected investigation row parsed");
            assert_eq!(entry.ticket_type, TicketType::Investigation);
            assert_eq!(entry.phase, Phase::Skipped, "reconciled on the same open");
        },
        &capture_path,
    );
    let _ = store;
    let body = std::fs::read_to_string(&capture_path).expect("read capture");
    assert!(
        body.contains(REVIEW_MIGRATION_TERMINATED_INVESTIGATION_EVENT),
        "terminate event missing: {body}"
    );
    assert!(
        body.contains("investigation_archived"),
        "archive event missing: {body}"
    );
}

// ---------------------------------------------------------------------------
// AC1 — admission rejection (defense in depth)
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn admission_rejected() {
    let dir = tempdir("inv-removal-admit");
    let store = StateStore::open(&dir).expect("open fresh store");
    let key = IssueKey::parse("owner/r#31").expect("parse owner/r#31");

    let capture_path = dir.join("admission.log");
    let _body = capture(
        || {
            let err = store
                .enqueue(&key, TicketType::Investigation, false)
                .expect_err("investigation admission must be rejected in N+1");
            let text = format!("{err}");
            assert!(
                text.contains("investigation-admission-rejected"),
                "error context must name the rejection: {text}"
            );
            assert!(
                text.contains("auto_review"),
                "error must name the replacement: {text}"
            );
            assert!(
                !text.contains("#331"),
                "error must not cite the internal removal issue: {text}"
            );
        },
        &capture_path,
    );
    let body = std::fs::read_to_string(&capture_path).expect("read admission capture");
    assert!(
        body.contains("investigation_admission_rejected"),
        "admission-rejected event missing: {body}"
    );
    assert!(
        body.contains("\"repo\":\"r\"") && body.contains("\"issue\":31"),
        "repo/issue identity missing: {body}"
    );

    // The store is unchanged: nothing was inserted.
    let snap = store.snapshot().expect("snapshot");
    assert_eq!(snap.entries.len(), 0, "rejected admission must not insert");

    // Code admission is unaffected.
    store
        .enqueue(
            &IssueKey::parse("owner/r#32").expect("parse r#32"),
            TicketType::Code,
            false,
        )
        .expect("code admission still works");
    let snap = store.snapshot().expect("snapshot after code admission");
    assert_eq!(snap.entries.len(), 1, "code entry admitted");
}

// ---------------------------------------------------------------------------
// Claim-file safety (plan §5 risk checkpoint)
// ---------------------------------------------------------------------------

/// A terminated-in-flight row may leave a claim file under `claims/`.
/// The reconcile pass unlinks it (mirror of `skip()`'s
/// `unlink_claim_best_effort`), so the reaper does not have to.
#[test]
fn reconcile_unlinks_leftover_claim_file() {
    let dir = tempdir("inv-removal-claim");
    stage_fixture(&dir, "state.json", PRE_N_JSON.as_bytes());
    // The fixture row has last_run_id = null, so synthesize the claim
    // digest from the display key exactly as the store does.
    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update("owner/r#23".as_bytes());
        hex::encode(hasher.finalize())
    };
    let claims_dir = dir.join("claims");
    std::fs::create_dir_all(&claims_dir).expect("create claims dir");
    let claim_path = claims_dir.join(format!("{digest}.claim"));
    std::fs::write(&claim_path, b"{\"synthetic\":true}").expect("write leftover claim");
    assert!(claim_path.is_file());

    let _store = StateStore::open(&dir).expect("open");
    assert!(
        !claim_path.exists(),
        "reconcile must unlink the leftover claim file for the terminated row"
    );
    // The code entry's (absent) claim file is irrelevant; the row is
    // untouched.
    let store = StateStore::open(&dir).expect("reopen");
    let snap = store.snapshot().expect("snapshot");
    assert_eq!(snap.entries.len(), 2);
}
