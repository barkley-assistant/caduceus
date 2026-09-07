# N-era fixture provenance

These files are **compatibility artefacts produced by the current
release-N code** (caduceus at the v9 SQLite / v2 JSON / review-sidecar
v1 era), not synthetic hand-written JSON.

- Generator: `tests/fixtures/state/generate_n_era.rs` (temporary,
  removed before the PR lands). It drives the CURRENT store APIs —
  `queue::StateStore::open` / `state::review::ReviewStore::open` /
  `ReviewStore::open_sqlite` — to write the exact bytes a fresh N-era
  daemon produces, then deletes nothing and ships only the artifacts
  listed below. Generation is deliberate (`cargo test --test
  generate_n_era -- --ignored`), never incidental; the fixtures are
  frozen compatibility artefacts (DAR §15).
- Naming (plan decision D3): the directory is `n-era/`, NOT `v8/` —
  the issue's "N-era-v8" label predates the #306 v8→v9 bump, and a
  version-pinned name would rot on the next schema bump. The fixture
  validation test asserts the artifacts open cleanly under CURRENT
  code, whatever version that is.
- Backend split (plan §4): the JSON-backend artifacts and the
  SQLite-backend artifacts were generated in separate state
  directories so the SHA-256-derived claim filenames (identical queue
  key across backends) never collide. Only the artifact files ship;
  the intermediate `claims/`, `review-claims/`, lock, and
  `daemon-identity` files do not.

Contents:

- `state.json` — v2 issue-queue envelope (current
  `QUEUE_FILE_VERSION`), written through `StateStore::open`'s own
  persist path (empty queue: the issue queue is the pre-review
  sibling and carries no rows in this fixture).
- `state.db` — SQLite at `schema_version = 9` (current
  `SCHEMA_VERSION`), containing the three review tables with one
  `review_state` row, one `review_history` row, and one
  `review_queue_entries` row (InProgress), WAL-checkpointed so no
  `-wal`/`-shm` sidecars ship.
- `review_queue.json` — v1 review queue sidecar, one InProgress
  entry (`owner/r#7` at head `bbbb2222…`).
- `review_state.json` — v1 review state sidecar, one Published
  `ReviewState` (verdict pass, sticky comment id 424242).
- `review_history.json` — v1 review history sidecar, one PASS row.

Validated by `tests/state/review_fixture_migration_test.rs`: each
artifact loads cleanly under current code and carries the current
envelope versions. These are the inputs #331's N+1 reconciliation
tests consume; the N+1-binary upgrade tests themselves are out of
scope here.
