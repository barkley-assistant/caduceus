# Review test fixtures + harnesses (issue #327)

Shared fixtures and harnesses for the Auto Review test suite. Spec:
`docs/architecture/auto-review.md` §4.4, §15, §16. Frozen migration
fixtures live in `tests/fixtures/state/` (not here); this file covers
the whole #327 fixture surface.

## Inventory

### `tests/fixtures/pr/*.json` — GitHub PR fixture set

Hand-authored JSON bodies matching `src/github/pr.rs`'s
`PullRequestDetail` wire shape (all-optional fields, `head.repo: null`
legal). Consumed via `include_str!` + `MockGitHub` mounts
(`tests/fixtures/github.rs`: `mount`, `mount_paged`, `mount_etag`,
`mount_status`, `mount_with`).

| DAR row | Fixture | Mount |
|---|---|---|
| open, non-draft, new SHA | `open.json` | `mount` |
| draft | `draft.json` | `mount` |
| fork (`head.repo != base.repo`) | `fork.json` | `mount` |
| closed | `closed.json` | `mount` |
| merged | `merged.json` | `mount` |
| `head.repo: null` (deleted head) | `nullable-head-repo.json` | `mount` |
| head moved since last poll | `force-pushed.json` | `mount` (new SHA vs persisted) |
| paginated list | `paginated-1.json`, `paginated-2.json` | `mount_paged` |
| malformed response | `malformed.json` | `mount` |
| rate-limited | `rate-limited.json` | `mount_status(429)` |

The daemon's PR list call is `GET /repos/{owner}/{repo}/pulls?state=all&per_page=100&sort=updated`
(`src/github/pr.rs` `list_pull_requests`); it follows `Link: rel="next"`
and caps pages. `mount_paged` matches page 1 with
`query_param_is_missing("page")`, so the daemon's extra query params
(`state`, `per_page`, `sort`) do not interfere with page routing (R4).

### `tests/fixtures/state/pre-n/` — frozen pre-N era (REAL)

REAL artifacts produced by the pre-Auto-Review (v7-era) binary at
commit `a89c5f2` — `state.json` (v1 issue envelope, 2 entries) and
`state.db` (SQLite v7, 2 queue rows, no review tables). Generation
method + provenance: `pre-n/ORIGIN.md`. Validated by
`tests/state/review_fixture_migration_test.rs` (v7 → v8 → v9 chain, v1
→ v2 JSON upgrade, issue data preserved).

### `tests/fixtures/state/n-era/` — frozen N-era (compatibility artefacts)

Current release-N shape: `state.json` (v2 issue envelope),
`state.db` (SQLite v9, review tables, one review_state row), and the
three v1 review sidecars (`review_queue.json`, `review_state.json`,
`review_history.json`). Generated ONCE by current code's own store
APIs (`generate_n_era.rs`, a temporary generator removed before the PR
lands); regeneration is deliberate, never incidental. Provenance:
`n-era/ORIGIN.md`. These are the inputs #331's N+1 reconciliation
tests consume; the N+1-binary upgrade tests are NOT built here (DAR
§15 acceptance split).

### `tests/fixtures/review_harness.py` — deterministic review worker harness

The review counterpart of `bridge_harness.py`: the seam a PR-review
worker occupies when the daemon spawns it. Responsibilities:

- Asserts the rendered `worker-prompt.md` carries the six DAR §7
  sections in fixed order (stable check; breaks on reorder).
- Reads the daemon-injected `ReviewResult v<N>` version from §2 and
  injects that exact version into the scripted result — a harness
  ahead of the daemon cannot produce results the daemon rejects.
- Writes a scripted `worker-result.json` selected by
  `FAKE_REVIEW_RESULT`: `pass`, `fail` (verdict-consistent FAIL with
  one blocking finding), `inconsistent` (FAIL + zero blocking — the
  daemon's validator rejects it as an execution failure, DAR §8),
  `malformed` (invalid JSON, exit 2), `oversized` (exceeds the
  daemon's `MAX_REVIEW_RESULT_FILE_BYTES` read cap, 4 MiB).

Self-test: `tests/plugin/review_harness_self_test.py` (pytest; drives
the harness as a subprocess exactly the way the daemon/bridge would).

## Naming decision (D3): era-based, not version-pinned

The issue's "N-era-v8" label predates the #306 v8→v9 bump. Fixtures
are labelled by ERA (`pre-n/`, `n-era/`), not by literal version: a
version-pinned name rots on the next schema bump. `n-era/` means
"what a fresh release-N daemon writes" — today v9 SQLite / v2 JSON /
sidecars v1; the fixture-validation test asserts the artifacts open
cleanly under CURRENT code, whatever version that is. Do NOT rename
these directories on the next bump; update ORIGIN.md instead.

## Release-N vs N+1 acceptance split (DAR §15)

- Release-N (this change): fixture exists and is valid +
  release-N load green on both backends
  (`tests/state/review_fixture_migration_test.rs`, 9 tests).
- N+1 (issue #331): the N+1 binary upgrades/reconciles these same
  frozen fixtures. NOT built here — do not add upgrade assertions to
  the release-N test binary.

## Regenerating fixtures

Pre-N and N-era fixtures are FROZEN. Regeneration is a deliberate
compatibility event (new era), not maintenance:

1. Pre-N: build the binary at the era commit, run it against a
   hermetic state dir + fake GitHub, snapshot `state.json` +
   `state.db` (WAL-checkpoint first), update `pre-n/ORIGIN.md`.
2. N-era: run the temporary generator (restore
   `tests/fixtures/state/generate_n_era.rs` + its Cargo.toml
   `[[test]]` entry), `N_ERA_OUT=<dir> cargo test --test
   generate_n_era -- --ignored --nocapture`, flatten the artifacts,
   update `n-era/ORIGIN.md`, remove the generator again.
3. Run `cargo test --test review_fixture_migration_test` — the
   committed-version sanity asserts (pre-N == v7, n-era == current)
   must still pass; if they fail, the fixtures are mislabelled.
