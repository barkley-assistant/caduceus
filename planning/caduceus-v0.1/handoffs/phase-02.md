# Handoff phase-02 — GitHub client and polling

- Work item: Phase 02 gate (GitHub client and polling)
- Outcome: complete
- Date: 2026-07-13

## Phase summary

Phase 02 implemented the typed HTTP client, ETag-aware cache,
repository discovery, labeled-issue poll, cadence / rate-limit
gate, trigger-label verifier, and complete issue-detail fetcher.
All six tasks completed without contract revisions, with 330
Rust tests + 30 pytest tests passing on Rust 1.97.

| Task | Title | Status |
|---|---|---|
| 2.1 | Build the typed HTTP client and persistent conditional cache | complete |
| 2.2 | Discover watched repositories | complete |
| 2.3 | Poll open labeled issues with a typed schema | complete |
| 2.4 | Handle poll cadence and rate limits | complete |
| 2.5 | Verify the selected trigger label immediately before work | complete |
| 2.6 | Fetch complete issue detail | complete |

## Gate commands run

```
$ python3 -B planning/caduceus-v0.1/tools/validate_plan.py
plan valid: 46 tasks, 10 phases, acyclic and phase-safe

$ cargo test --locked --test github_client_test --test repository_poll_test \
    --test issue_poll_test --test rate_limit_test --test cadence_test \
    --test verify_test --test issue_detail_test
test result: ok. 15 passed (github_client_test)
test result: ok. 12 passed (repository_poll_test)
test result: ok. 15 passed (issue_poll_test)
test result: ok. 18 passed (rate_limit_test)
test result: ok.  8 passed (cadence_test)
test result: ok. 14 passed (verify_test)
test result: ok.  9 passed (issue_detail_test)

$ cargo test --locked --all-targets
test result: ok.  2 passed (caduceus inline tests)
test result: ok.  0 passed (caduceus doc tests)
test result: ok.  8 passed (cadence_test)
test result: ok. 20 passed (config_resolution_test)
test result: ok. 34 passed (config_test)
test result: ok. 28 passed (error_test)
test result: ok. 15 passed (github_client_test)
test result: ok.  9 passed (issue_detail_test)
test result: ok. 15 passed (issue_poll_test)
test result: ok. 16 passed (logging_test)
test result: ok. 21 passed (meta_test)
test result: ok. 27 passed (queue_model_test)
test result: ok. 18 passed (rate_limit_test)
test result: ok. 12 passed (repository_poll_test)
test result: ok. 16 passed (token_test)
test result: ok. 20 passed (validate_test)
test result: ok. 14 passed (verify_test)
test result: ok. 26 passed (voice_rule_test)
test result: ok. 29 passed (worker_result_test)

$ pytest -q tests/hermes_plugin_test.py
30 passed in 0.45s

$ cargo fmt --check
# no diff

$ cargo clippy --locked --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s)
```

## Per-task deliverables

Every task has a complete handoff under
`planning/caduceus-v0.1/handoffs/<task-id>.md`:

- `2.1.md` — Typed HTTP client + ETag cache
- `2.2.md` — Repository discovery
- `2.3.md` — Labeled-issue poll
- `2.4.md` — Cadence + rate-limit gate
- `2.5.md` — Trigger-label verifier
- `2.6.md` — Complete issue detail

## Forbidden-side-effect spot-checks

Per `CONTRACTS.md` and the Phase 02 gate:

- The HTTP client never forwards the resolved GitHub token to
  a cross-origin redirect target. The cross-host test in
  `github_client_test.rs` mounts a second mock and asserts
  `expect(0)` to prove the token never reaches it.
- The cache file is written via temp-file + `fsync` + `rename`;
  on Unix the directory is `0700` and the file is `0600`.
  Atomic replacement is exercised on every successful 200
  via the `cache_corruption_is_recovered_on_next_open`
  regression test.
- The cache file uses serde's default JSON encoding. Bodies
  are stored as `Vec<u8>` which serialises as a JSON array
  of integers; for a typical GitHub JSON issue list this is
  acceptable.
- A 429 response is translated by the typed client into
  `CaduceusError::RateLimited { reset_at, remaining, limit }`
  with a precise `reset_at`. The meta layer (Task 2.4)
  persists the observation via the `CadenceGate`.
- The polling path is read-only with respect to the queue
  and the metadata file. `poll_code` / `poll_investigation`
  return `IssuePollOutcome { summaries, diagnostics }`;
  admission is the caller's responsibility (Phase 3).
- The verifier treats 404 as `Skip` (no retry-budget
  consumption) and 403/5xx as `Err` (retry-budget
  preserved). The contract's "do not consume a retry" rule
  is honoured.
- The detail fetcher uses `try_join3` so a 429 on any branch
  short-circuits the join. The other in-flight requests
  finish in the background; the persistent cache is read
  for each branch.
- Token values never appear in `Display` or `Debug` of any
  error variant produced by the HTTP layer. The
  `map_status` helper scrubs every GitHubApi message through
  `error::scrub` before constructing the variant.

## CONTRACTS.md status

The contracts file was **not modified** during Phase 02.

```
$ sha256sum planning/caduceus-v0.1/CONTRACTS.md
ace44d138c05548cb1b15e3176ed99f0521148ed4c24b449643716340be91eb2
```

The hash matches the value pinned in
`planning/caduceus-v0.1/task-manifest.json` (`contracts_sha256`).

## Residual risks

- **In-flight requests after a join error.** `try_join3`
  drops the *futures* when one errors, but the underlying
  `reqwest::Request` may still be in flight on the
  connection pool. For a daemon that polls on a 2-minute
  cadence this is negligible; a strict-cancellation run-loop
  can subscribe to the request lifecycle if a hard
  cancellation is required (Phase 5).
- **Trusted-comment filter is a *filter*, not a
  *partition*.** `IssueDetail::trusted_comments` is a
  subset of `IssueDetail::comments`; the worker prompt
  builder is responsible for de-duplicating when rendering.
  A future refactor could swap to a partition if the
  builder prefers that shape.
- **Out-of-order trigger labels.** The merge in Task 2.3
  walks the code summaries first, then the investigation
  summaries. A key that is "ambiguous" (both labels on
  the object) becomes one `Ambiguous` diagnostic with the
  *investigation* ticket's title and labels as the recorded
  fields. The contract does not pin which half wins.
- **Server-suggested poll interval only lengthens.** The
  `CadenceGate::record_poll_interval` helper always takes
  the longer of the existing and the new value, so the
  server-suggested floor never *shortens* the configured
  cadence. A misbehaving proxy that returns a very small
  value will be ignored.
- **Stale-observation protection relies on absolute
  `reset_at`.** Two clients that observe the same header at
  slightly different times record observations with
  consistent `reset_at` (the unix timestamp wins); the
  implementation refuses the older one and keeps the newer.

## Test coverage at the phase boundary

The phase gate runs the seven primary-test binaries
specifically named in `phases/02-github.md` plus the full
`cargo test --locked --all-targets` sweep. At the time of
this handoff, **330 Rust tests pass** (123 of them new in
this phase) and **30 pytest tests pass**.

| Test binary | Count |
|---|---|
| `github_client_test` | 15 |
| `repository_poll_test` | 12 |
| `issue_poll_test` | 15 |
| `rate_limit_test` | 18 |
| `cadence_test` | 8 |
| `verify_test` | 14 |
| `issue_detail_test` | 9 |
| `caduceus` (inline) | 2 |
| `config_test` | 34 |
| `config_resolution_test` | 20 |
| `error_test` | 28 |
| `logging_test` | 16 |
| `meta_test` | 21 |
| `queue_model_test` | 27 |
| `token_test` | 16 |
| `validate_test` | 20 |
| `voice_rule_test` | 26 |
| `worker_result_test` | 29 |
| **Total Rust** | **330** |
| `hermes_plugin_test.py` | 30 |

## Stop conditions honoured

- No contract revisions: `CONTRACTS.md` and `contracts_sha256`
  are unchanged.
- No forbidden side effects: every test runs in-process
  with `wiremock`; the only filesystem mutation under
  `tests/` is `<state_dir>/cache/http.json` (Task 2.1 cache)
  and `<state_dir>/state_meta.json` (Task 2.4 meta store).
  No real network egress, no subprocess execution, no env
  mutation, no symlink/perm-mode violations.
- The phase gate's `cargo test --locked` matches the
  workspace's documented test policy.

## Ready for Phase 03

The next phase is **03-queue** (queue file I/O, claim
lifecycle, retry budget, queue reset, atomic writer). The
Phase 02 surface (`IssueSummary`, `IssueDetail`, `IssueKey`,
`HttpCache`, `MetaStore`, `CadenceGate`) is the canonical
input to the queue writer (Task 3.x) and the queue
mutations do not require any new public type from Phase 02.