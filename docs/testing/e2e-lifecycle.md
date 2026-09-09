# E2E PR lifecycle test (issue #322)

`tests/review/lifecycle_test.rs` is the end-to-end PR review lifecycle
test: ONE full lifecycle with **no stubs**, running on BOTH state
backends. It closes the DAR §15 "End-to-end" row.

## What the test proves

The full pipeline, driven through the real production code paths:

1. **Discovery** admits an eligible PR (`owner/r#7`, head `B`) and
   assigns generation 1.
2. **Claim + dispatch** runs the REAL `review_harness.py` worker as a
   REAL subprocess under the REAL `caduceus __worker-supervisor`
   supervision protocol; the worker reads the REAL prompt the daemon
   rendered, asserts the DAR §7 section order, and writes the REAL
   result file (`<worktree>/worker-result.json`). The verdict is
   scripted FAIL.
3. **Finalizer** publishes the FAIL verdict as the sticky comment
   (exactly one `POST`).
4. **Head moves** (wire row force-push `B → C`): discovery observes a
   stale SHA and admits generation 2.
5. **Re-review** runs the harness again (scripted PASS) and the
   finalizer **updates the SAME comment** (one `PATCH`, no second
   `POST`).

Asserted (acceptance criteria from issue #322):

- Lifecycle green end-to-end with no stubs on JSON **and** SQLite.
- Exactly one sticky comment across both revisions (`sticky_comment_id`
  unchanged); `review_history` holds two rows with distinct run ids.
- Same-SHA re-poll produces zero new admissions
  (`review_skipped_already_complete`); publish-failure resume publishes
  the same durable result WITHOUT re-running the worker.

Also covered:

- **Out-of-order completion** (DAR §9.4): generation 2 completes before
  generation 1. The sticky comment shows the newer result; the older
  result persists in history only, and a stale `DueFinalization` is
  suppressed with `review_publication_suppressed_stale_generation`.
- **Publish-failure resume** (DAR §9.1): a GitHub 500 during
  publication leaves the row at `FailedRetryable` with persisted
  backoff; the resume publishes without a model re-run.

## Harness design (no stubs)

The only controlled fake is the GitHub API (wiremock `MockGitHub`).
Everything else runs for real:

| Component | Implementation |
|---|---|
| GitHub API | wiremock (`tests/fixtures/github.rs`) |
| Git origin | real local bare remote (`file://`), real commits, real merge-base |
| Daemon dispatch | `run_review_claim_for_tests` (`per_review.rs`), real mirror/worktree/diff/prompt/digest code |
| Executor | the REAL `supervise()` production function re-execing the REAL `caduceus` binary (`ReleaseBinary::locate()`) in `__worker-supervisor` mode |
| Worker | the REAL `tests/fixtures/review_harness.py` as a subprocess |
| Finalizer | `poll_publication_step_for_tests` / `finalize_review` (`review_finalize_step.rs`, `finalize.rs`) |
| Store | `ReviewStore::open` (JSON) / `ReviewStore::open_sqlite` (SQLite) |

### Why the executor deviates from `Services::production`

`run_review_claim` pins the supervisor's `self_exe` to
`std::env::current_exe()`. Inside a test binary that is the libtest
harness, which does not implement the hidden `__worker-supervisor`
command. The harness therefore implements `Executor` with the same body
as `TrustedHostExecutor` but passes `ReleaseBinary::locate()` — the
same `caduceus` binary production re-execs — as `self_exe`. The
supervisor subprocess, the framed control protocol, the sanitized
worker env, and the worker subprocess are all the real production
machinery.

### Why `FAKE_REVIEW_RESULT` rides the worker argv

The production supervisor builds the worker environment with an EMPTY
allowlist (`src/main.rs` `run_supervisor_mode`), so a
`worker_env_allowlist` entry is dropped by `env_clear()`. The harness
therefore selects the scripted result through the worker command:

```toml
worker_command = ["env", "FAKE_REVIEW_RESULT=fail", "python3", "<abs>tests/fixtures/review_harness.py"]
```

`/usr/bin/env` survives the sanitized environment (PATH is a default
allowlist entry) and injects the selector for the harness's own
process. The harness still runs as a real subprocess and still asserts
the real prompt's §1–§6 order and schema version before writing the
real result file.

### Why the wire SHAs are real commits

The #327 wire fixtures (`tests/fixtures/pr/open.json`,
`force-pushed.json`) carry literal SHAs (`bbbb2222…`, `cccc3333…`)
that do not exist in the local remote. The harness builds the bare
remote FIRST, then injects the real commit SHAs (`base`/`mid`/`tip`)
into the wire rows, so admission's SHA-anchored fetches and
`git merge-base` work against the real mirror.

### Re-mount semantics

wiremock serves the first matching mount when priorities tie. The
force-push re-mount therefore uses wiremock priority `1`
(`mount_status_priority`, added to `tests/fixtures/github.rs`) so the
newer `/pulls` body beats the original default-priority mount. The
publish-failure recovery mount uses the same priority mechanism.

## Test list

| Test | Backends | Coverage |
|---|---|---|
| `lifecycle_json` / `lifecycle_sqlite` | one each | AC1 + AC2 full FAIL→PASS lifecycle |
| `lifecycle_out_of_order` | both | DAR §9.4 B-before-A suppression |
| `lifecycle_same_sha_repoll` | both | AC3 part 1: zero new admissions |
| `lifecycle_publish_failure_resume` | both | AC3 part 2: no model re-run |

## Running the test

The harness re-execs the real `caduceus` binary, so the binary must be
built first:

```bash
cargo build --bins
cargo test --test lifecycle_test
```

All lifecycle tests are `#[serial_test::serial]` (wiremock + the local
git remote do not parallelize cleanly). In CI the full gate
(`cargo test --locked --all-targets`) builds the binary and runs this
suite as part of `--all-targets`.

## Files

- `tests/review/lifecycle_test.rs` — the test binary (deliverable).
- `tests/review/lifecycle_harness.rs` — the shared harness module.
- `tests/fixtures/github.rs` — `mount_status_priority` helper (the
  only shared-fixture change).
- `Cargo.toml` — the `lifecycle_test` `[[test]]` entry.