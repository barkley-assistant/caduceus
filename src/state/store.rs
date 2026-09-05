//! Versioned SQLite state store — the v1 runtime backend.
//!
//! Every stateful operation (queue, metadata, claims, checkpoints,
//! circuit breakers) goes through this store. The schema is versioned
//! so the daemon can refuse an unknown schema before mutating data.
//!
//! ## Schema (v1)
//!
//! - `schema_version` — single-row version table (envelope check).
//! - `queue_entries` — one row per tracked issue (replaces
//!   `state.json`).
//! - `state_meta` — arbitrary key-value metadata (replaces
//!   `state_meta.json`).
//! - `claims` — per-issue lease tokens (replaces files under
//!   `<state_dir>/claims/`).
//! - `checkpoints` — durable finalization checkpoints per run.
//! - `circuit_breakers` — per-issue failure tracking.

use std::path::Path;

use rusqlite::{params, Connection, Transaction};

use crate::infra::error::{store_version_guidance, CaduceusError, CaduceusResult};

/// Current schema version. Bumping it is a breaking change — the
/// store refuses to open a database with a *higher* version.
///
/// ## v2
///
/// - `checkpoints` table gains `operation_id TEXT` and
///   `remote_marker TEXT` columns for durable operation IDs and
///   remote reconciliation markers. Existing rows get NULL.
///
/// ## v3
///
/// - `leases` table for per-issue fenced leases with fencing
///   tokens, owner tracking, and expiry.
///
/// ## v4
///
/// - `circuit_state` table replaces dead `circuit_breakers` table.
///   Keyed by `(scope, scope_id)` for per-provider and
///   per-repository circuit state.
///
/// ## v5
///
/// - `oci_runs` table for per-container lifecycle state tracking.
///   Keyed by `run_id` with indices on `container_id`, `daemon_id`,
///   and `state`.
///
/// ## v6
///
/// - `queue_entries` gains `blocked_source TEXT` and
///   `blocked_recovery_hint TEXT` columns for terminal refuse-to-
///   operate metadata. Existing rows get NULL defaults.
///
/// ## v7
///
/// - OCI run identity and installation attribution are now part of the
///   supported state contract. There is intentionally no v6 -> v7
///   migration: v6 state must be reinitialised rather than being read
///   with different lifecycle semantics.
///
/// ## v8 (not yet live — armed by #295)
///
/// - The review-era structures activate as v8 atomically with #295 in
///   ONE commit: append a `Migration { from: 7, to: 8, label:
///   "review-era structures", apply: m_v7_v8 }` entry to
///   `SQLITE_MIGRATIONS`, bump `SCHEMA_VERSION` to 8, bump
///   `QUEUE_FILE_VERSION` to 2 (with the JSON migration step), and add
///   the v8 review tables to the schema DDL. `assert_registry_wellformed`
///   rejects any step whose `to` exceeds `SCHEMA_VERSION`, so the
///   registry entry and the version bump cannot land separately.
pub const SCHEMA_VERSION: i64 = 7;

/// The last schema version that is deliberately rejected instead of
/// migrated. Keeping this explicit prevents a future schema bump from
/// accidentally turning the breaking v6 boundary into a silent upgrade.
pub const STALE_SCHEMA_VERSION: i64 = 6;

/// Name of the SQLite database file inside the state directory.
pub const DB_FILENAME: &str = "state.db";

// Schema DDL (applied atomically at open time).

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS schema_version (
    version    INTEGER NOT NULL,
    migrated_at TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS queue_entries (
    issue_key     TEXT PRIMARY KEY,
    phase         TEXT NOT NULL,
    ticket_type   TEXT NOT NULL,
    attempts      INTEGER NOT NULL DEFAULT 0,
    last_error    TEXT,
    last_run_id   TEXT,
    next_attempt_at TEXT,
    finalization  TEXT,
    queued_at     TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    generation    INTEGER NOT NULL DEFAULT 1,
    blocked_source TEXT,
    blocked_recovery_hint TEXT
);

CREATE TABLE IF NOT EXISTS state_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS claims (
    claim_id   TEXT PRIMARY KEY,
    issue_key  TEXT NOT NULL,
    worker_pid INTEGER,
    token      TEXT NOT NULL,
    claimed_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    FOREIGN KEY (issue_key) REFERENCES queue_entries(issue_key)
);

CREATE TABLE IF NOT EXISTS checkpoints (
    run_id          TEXT NOT NULL,
    stage           TEXT NOT NULL,
    checkpoint_data TEXT,
    created_at      TEXT NOT NULL,
    operation_id    TEXT,
    remote_marker   TEXT,
    PRIMARY KEY (run_id, stage)
);

CREATE TABLE IF NOT EXISTS circuit_state (
    scope TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'closed',
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    last_failure_at INTEGER,
    opened_at INTEGER,
    last_probe_at INTEGER,
    PRIMARY KEY (scope, scope_id)
);

CREATE TABLE IF NOT EXISTS leases (
  issue_key  TEXT PRIMARY KEY,
  owner_id  TEXT NOT NULL,
  fencing_token INTEGER NOT NULL,
  expires_at  INTEGER NOT NULL,
  state  TEXT NOT NULL CHECK(state IN ('held', 'released', 'expired'))
);

CREATE TABLE IF NOT EXISTS oci_runs (
  run_id  TEXT PRIMARY KEY,
  container_id  TEXT,
  state  TEXT NOT NULL,
  engine  TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  daemon_id TEXT NOT NULL,
  issue_id  TEXT NOT NULL,
  worker_command_sha256 TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_oci_runs_container_id ON oci_runs(container_id);
CREATE INDEX IF NOT EXISTS idx_oci_runs_daemon_id ON oci_runs(daemon_id);
CREATE INDEX IF NOT EXISTS idx_oci_runs_state ON oci_runs(state);
";

// Open / initialise.

/// Open or create a versioned SQLite database at `path`. If the
/// database is new it is initialised with the current schema. If
/// it already exists the schema version is checked:
///
/// - Equal to [`SCHEMA_VERSION`] → open, apply any missing tables.
/// - Higher than [`SCHEMA_VERSION`] → reject with
///   [`CaduceusError::StoreVersionUnsupported`] (future schema, must
///   upgrade or reinitialise).
/// - Equal to [`STALE_SCHEMA_VERSION`] → reject with
///   [`CaduceusError::StateCorrupt`] (stale v6, must reinitialise).
/// - Lower → run the migration chain ([`SQLITE_MIGRATIONS`]).
///
/// The connection uses WAL mode for concurrent reads and is created
/// with `PRAGMA journal_mode=WAL`.
pub fn open(path: &Path) -> CaduceusResult<Connection> {
    let db_path = path.to_path_buf();
    let conn = Connection::open(path).map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.clone(),
        message: format!("cannot open SQLite store at {}: {e}", path.display()),
    })?;

    // Enable WAL mode for read concurrency.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot set pragmas: {e}"),
        })?;

    // Check / initialise schema version.
    let table_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot probe schema_version table: {e}"),
        })?;

    if table_count == 0 {
        // Fresh database — initialise schema.
        init_schema(&conn, &db_path)?;
    } else {
        // Existing database — check version.
        let existing_version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .map_err(|e| CaduceusError::StateCorrupt {
                path: db_path.clone(),
                message: format!("cannot read schema_version: {e}"),
            })?;

        if existing_version > SCHEMA_VERSION {
            return Err(CaduceusError::StoreVersionUnsupported {
                backend: "sqlite",
                path: db_path,
                found: existing_version,
                supported: SCHEMA_VERSION,
                guidance: store_version_guidance(true),
            });
        }

        if existing_version == STALE_SCHEMA_VERSION {
            return Err(CaduceusError::StateCorrupt {
                path: db_path,
                message: format!(
                    "SQLite store has stale schema v6; v6 state is not migrated and must be reinitialized with fresh state. {}",
                    store_version_guidance(false)
                ),
            });
        }

        if existing_version < SCHEMA_VERSION {
            // Run the migration chain from existing_version toward
            // SCHEMA_VERSION. A step fires iff the store's current
            // version equals the step's `from`; one transaction + one
            // schema_version row write per step keeps the chain
            // idempotent and crash-resumable (D2).
            let mut current = existing_version;
            for step in SQLITE_MIGRATIONS {
                if current == step.from {
                    run_migration_step(&conn, &db_path, step)?;
                    current = step.to;
                }
            }
            if current < SCHEMA_VERSION {
                tracing::warn!(
                    from = current,
                    to = SCHEMA_VERSION,
                    "migration registry gap: chain reaches v{current}, below SCHEMA_VERSION v{SCHEMA_VERSION}; applying schema and recording the current version"
                );
                apply_schema(&conn, &db_path)?;
                record_version(&conn, &db_path)?;
            }
        }

        // Ensure missing tables are created (idempotent).
        apply_schema(&conn, &db_path)?;
    }

    Ok(conn)
}

fn init_schema(conn: &Connection, db_path: &Path) -> CaduceusResult<()> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.to_path_buf(),
            message: format!("cannot start init transaction: {e}"),
        })?;

    apply_schema_in_tx(&tx, db_path)?;
    record_version_in_tx(&tx, db_path)?;

    tx.commit().map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.to_path_buf(),
        message: format!("cannot commit init transaction: {e}"),
    })?;

    Ok(())
}

fn apply_schema(conn: &Connection, db_path: &Path) -> CaduceusResult<()> {
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.to_path_buf(),
            message: format!("cannot apply schema: {e}"),
        })
}

fn apply_schema_in_tx(tx: &Transaction, db_path: &Path) -> CaduceusResult<()> {
    tx.execute_batch(SCHEMA_SQL)
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.to_path_buf(),
            message: format!("cannot apply schema in tx: {e}"),
        })
}

fn record_version(conn: &Connection, db_path: &Path) -> CaduceusResult<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO schema_version (version, migrated_at) VALUES (?1, ?2)",
        params![SCHEMA_VERSION, now],
    )
    .map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.to_path_buf(),
        message: format!("cannot record schema version: {e}"),
    })?;
    Ok(())
}

fn record_version_in_tx(tx: &Transaction, db_path: &Path) -> CaduceusResult<()> {
    let now = chrono::Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO schema_version (version, migrated_at) VALUES (?1, ?2)",
        params![SCHEMA_VERSION, now],
    )
    .map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.to_path_buf(),
        message: format!("cannot record schema version in tx: {e}"),
    })?;
    Ok(())
}

/// One structural migration step in the SQLite chain (DAR §4.4).
///
/// The chain is declarative data: a step fires iff the store's current
/// `schema_version` equals `from`, runs inside ONE transaction, and
/// writes `to` to the `schema_version` row in that same transaction.
/// Structural-only — a step whose behaviour depends on which binary
/// opens the store is a version-semantics violation.
pub(crate) struct Migration {
    /// Version this step takes the store FROM (must equal the current
    /// `schema_version` row when it fires).
    pub from: i64,
    /// Version this step takes the store TO (written to the
    /// `schema_version` row in the same transaction as the DDL).
    pub to: i64,
    /// Human label used in logs and the test harness.
    pub label: &'static str,
    /// Structural DDL / data transform, executed inside one transaction.
    pub apply: fn(&Transaction) -> CaduceusResult<()>,
}

/// The declarative migration chain (D1/D4).
///
/// The chain deliberately ends at v6: v6 is the stale-reinitialise
/// boundary ([`STALE_SCHEMA_VERSION`]) and v8 does not exist yet — #295
/// arms v7→v8 by appending one entry + bumping [`SCHEMA_VERSION`] in
/// the same commit.
const SQLITE_MIGRATIONS: &[Migration] = &[
    Migration {
        from: 1,
        to: 2,
        label: "checkpoints operation columns",
        apply: m_v1_v2,
    },
    Migration {
        from: 2,
        to: 3,
        label: "leases table (apply_schema)",
        apply: m_v2_v3,
    },
    Migration {
        from: 3,
        to: 4,
        label: "circuit_state replaces circuit_breakers",
        apply: m_v3_v4,
    },
    Migration {
        from: 4,
        to: 5,
        label: "oci_runs table (apply_schema)",
        apply: m_v4_v5,
    },
    Migration {
        from: 5,
        to: 6,
        label: "queue_entries blocked columns",
        apply: m_v5_v6,
    },
];

/// Validate the registry's chain invariants (D1/D4): steps are strictly
/// increasing and contiguous (`from[i] == to[i-1]`), the chain starts
/// at v1, no step migrates FROM the stale boundary
/// [`STALE_SCHEMA_VERSION`], and no step's `to` exceeds
/// [`SCHEMA_VERSION`]. The last clause makes arming v8 (a
/// `from: 7, to: 8` entry) impossible without bumping
/// [`SCHEMA_VERSION`] in the same change (#295).
pub fn assert_registry_wellformed() -> CaduceusResult<()> {
    let steps = SQLITE_MIGRATIONS;
    if steps.is_empty() {
        return Err(CaduceusError::Other(
            "migration registry must not be empty".to_string(),
        ));
    }
    if steps[0].from != 1 {
        return Err(CaduceusError::Other(format!(
            "migration registry must start at v1, found v{}",
            steps[0].from
        )));
    }
    for (i, step) in steps.iter().enumerate() {
        if step.from >= step.to {
            return Err(CaduceusError::Other(format!(
                "migration registry step {i} ({}) is not upward: v{} → v{}",
                step.label, step.from, step.to
            )));
        }
        if step.from == STALE_SCHEMA_VERSION {
            return Err(CaduceusError::Other(format!(
                "migration registry step {i} ({}) would migrate the stale v{STALE_SCHEMA_VERSION} boundary, which is reinitialise-only by policy",
                step.label
            )));
        }
        if step.to > SCHEMA_VERSION {
            return Err(CaduceusError::Other(format!(
                "migration registry step {i} ({}) targets v{} above SCHEMA_VERSION v{SCHEMA_VERSION}; bump SCHEMA_VERSION in the same change",
                step.label, step.to
            )));
        }
        if i > 0 && step.from != steps[i - 1].to {
            return Err(CaduceusError::Other(format!(
                "migration registry gap at step {i} ({}): starts at v{} but previous step ends at v{}",
                step.label,
                step.from,
                steps[i - 1].to
            )));
        }
    }
    Ok(())
}

/// Expose the migration registry as `(from, to, label)` triples so
/// tests (and #295/#327) can assert chain invariants and iterate the
/// chain without reaching into the registry internals.
pub fn sqlite_migration_chain() -> Vec<(i64, i64, &'static str)> {
    SQLITE_MIGRATIONS
        .iter()
        .map(|migration| (migration.from, migration.to, migration.label))
        .collect()
}

/// Execute one registry step inside its own transaction, then write the
/// step's target version into `schema_version` in the same transaction
/// (D2: one step = one transaction = one visible version bump). A crash
/// between steps leaves the store at the last committed version; the
/// next open re-enters the chain from there.
fn run_migration_step(conn: &Connection, db_path: &Path, step: &Migration) -> CaduceusResult<()> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.to_path_buf(),
            message: format!("cannot start migration transaction ({}): {e}", step.label),
        })?;
    (step.apply)(&tx).map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.to_path_buf(),
        message: format!("{} migration failed: {e}", step.label),
    })?;
    let now = chrono::Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO schema_version (version, migrated_at) VALUES (?1, ?2)",
        params![step.to, now],
    )
    .map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.to_path_buf(),
        message: format!("cannot record schema version {}: {e}", step.to),
    })?;
    tx.commit().map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.to_path_buf(),
        message: format!("cannot commit migration transaction ({}): {e}", step.label),
    })?;
    Ok(())
}

/// Migrate from schema v1 to v2 by adding the `operation_id` and
/// `remote_marker` columns to the `checkpoints` table. Both columns
/// are nullable so existing rows get NULL defaults.
fn m_v1_v2(tx: &Transaction) -> CaduceusResult<()> {
    tx.execute_batch(
        "ALTER TABLE checkpoints ADD COLUMN operation_id TEXT;
         ALTER TABLE checkpoints ADD COLUMN remote_marker TEXT;",
    )
    .map_err(|e| CaduceusError::Other(format!("v1→v2 ALTER checkpoints: {e}")))?;
    Ok(())
}

/// Migrate from schema v2 to v3. The `leases` table is created by
/// `apply_schema`, so this is a no-op migration that exists for
/// the migration wiring convention.
fn m_v2_v3(tx: &Transaction) -> CaduceusResult<()> {
    // The `leases` table is created by `apply_schema` via `SCHEMA_SQL`.
    // No ALTER TABLE statements are needed for v2→v3.
    let _ = tx;
    Ok(())
}

/// Migrate from schema v3 to v4. Drops the dead `circuit_breakers`
/// table and creates the new `circuit_state` table via `apply_schema`.
fn m_v3_v4(tx: &Transaction) -> CaduceusResult<()> {
    tx.execute_batch("DROP TABLE IF EXISTS circuit_breakers;")
        .map_err(|e| CaduceusError::Other(format!("v3→v4 DROP circuit_breakers: {e}")))?;
    Ok(())
}

/// Migrate from schema v4 to v5. The `oci_runs` table is created by
/// `apply_schema`, so this is a no-op migration that exists for the
/// migration wiring convention.
fn m_v4_v5(tx: &Transaction) -> CaduceusResult<()> {
    // The `oci_runs` table is created by `apply_schema` via `SCHEMA_SQL`.
    // No ALTER TABLE statements are needed for v4→v5.
    let _ = tx;
    Ok(())
}

/// Migrate from schema v5 to v6 by adding `blocked_source` and
/// `blocked_recovery_hint` columns to `queue_entries`. Both columns
/// are nullable so existing rows get NULL defaults.
fn m_v5_v6(tx: &Transaction) -> CaduceusResult<()> {
    tx.execute_batch(
        "ALTER TABLE queue_entries ADD COLUMN blocked_source TEXT;
         ALTER TABLE queue_entries ADD COLUMN blocked_recovery_hint TEXT;",
    )
    .map_err(|e| CaduceusError::Other(format!("v5→v6 ALTER queue_entries: {e}")))?;
    Ok(())
}

/// Open or create the database under `state_dir`.
pub fn open_in(state_dir: &Path) -> CaduceusResult<Connection> {
    let path = state_dir.join(DB_FILENAME);
    open(&path)
}
