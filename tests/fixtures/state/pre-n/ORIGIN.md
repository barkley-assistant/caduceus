# Pre-N fixture provenance

These files are **REAL artifacts produced by the pre-Auto-Review
(v7-era) caduceus binary**, not synthetic constructions.

- Binary: built from commit `a89c5f2` ("build(executor): implement
  target-neutral WorkTarget boundary (issue #346) (#358)") — the parent
  of `d7111ff` (feat(state): add review stores with atomic v8
  activation, #295). `SCHEMA_VERSION = 7`, `QUEUE_FILE_VERSION = 1`,
  no review tables, no review sidecar files.
- Toolchain: rustc 1.97.1 (the era-matching rustup toolchain), debug
  profile, `cargo build --bin caduceus --locked`.
- Generation: two full daemon ticks (`caduceus run`) against a hermetic
  environment — a loopback fake GitHub API server (one `autofix` issue
  `owner/r#17` and one `autofix-investigate` issue `owner/r#23`) and a
  local git clone whose origin parses to the same loopback api host.
  The workers (`/bin/echo`) produce no `worker-result.json`, so each
  entry goes through exactly one claim/fail cycle (attempts = 1) and
  returns to `Queued` with a retry backoff — a realistic mid-life
  pre-N queue. `state_backend: json` produced `state.json`; a second
  run with `state_backend: sqlite` produced `state.db`.
- Snapshot: straight copy of the two files after the second tick. The
  SQLite file was checkpointed (`PRAGMA wal_checkpoint(TRUNCATE)`) so
  no `-wal`/`-shm` sidecars ship.
- No review-era structures exist in either file: the v7 binary has no
  review code.

Contents:

- `state.json` — v1 envelope (`"version": 1`), two entries
  (`owner/r#17` code, `owner/r#23` investigation), both `queued`.
- `state.db` — SQLite `schema_version = 7`, v7 table set only
  (queue_entries, state_meta, claims, checkpoints, circuit_state,
  leases, oci_runs), same two rows in `queue_entries`.

Validated by `tests/state/review_fixture_migration_test.rs`: the JSON
file loads and upgrades v1 → current via `parse_queue_state` /
`StateStore::open`, and the SQLite file runs the full v7 → v8 → v9
migration chain via `open()`, preserving both issue rows.
