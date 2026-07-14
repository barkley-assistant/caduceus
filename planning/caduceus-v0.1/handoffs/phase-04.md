# Handoff phase-04 — Repository and worktree lifecycle

- Work item: Phase 04 gate (Repository and worktree lifecycle)
- Outcome: complete
- Date: 2026-07-14

## Phase summary

Phase 04 made every per-run worktree lifecycle decision safe
and idempotent under the daemon's single-host, one-shot
model. The `GitRunner` from Task 4.1 stayed untouched in
shape (process-group isolation, prompt suppression, timeout,
credential scrubbing); Task 4.2 filled in `create` end-to-end
and renamed `WorktreeHandle` → `Worktree` to match the
`&Worktree` reference Task 5.1 will eventually consume;
Task 4.3 filled in `remove` with the documented safety
checks. The `gc` CLI entry is the only Phase 4 surface that
remains a stub — it lands with the `caduceus worktree-gc`
CLI in a later phase.

All three Phase 4 tasks completed without contract
revisions, with 47 new Rust tests bringing the per-phase
acceptance count to 27 (4.1) + 11 (4.2) + 9 (4.3) = 47.

| Task | Title | Status | Handoff |
|---|---|---|---|
| 4.1 | Discover and validate local clones | complete | `handoffs/4.1.md` |
| 4.2 | Create a daemon-owned worktree and branch | complete | `handoffs/4.2.md` |
| 4.3 | Tear down safely | complete | `handoffs/4.3.md` |

## Files changed across the phase

| Path | Total role |
|---|---|
| `src/worktree.rs` | Hosts `Worktree`, `GitRunner`, `RepositoryInfo`, `find_main_clone`, `create`, `remove`; renamed `WorktreeHandle` → `Worktree`; added pre-flight collision detection, `git check-ref-format --branch` validation, the `fs2` flock on `<repo>/.worktrees/.lock`, branch-retention decision logic, and `should_retain_branch` / `path safety` helpers. |
| `src/lib.rs` | Re-exports `Worktree`, `create_worktree`, `remove_worktree`, plus the discovery surface. |
| `tests/repository_discovery_test.rs` | Task 4.1 — 27 tests. |
| `tests/worktree_create_test.rs` | Task 4.2 — 11 tests. |
| `tests/worktree_remove_test.rs` | Task 4.3 — 9 tests. |
| `planning/caduceus-v0.1/handoffs/4.{1,2,3}.md` | Per-task handoffs. |
| `planning/caduceus-v0.1/progress.json` | Updated by the controller on each task transition. |

`Cargo.toml` and `Cargo.lock` were left untouched — the
`fs2` dependency that Task 4.2 uses was already declared
in Phase 4.1.

## Gate commands run

```text
$ python3 -B planning/caduceus-v0.1/tools/validate_plan.py
plan valid: 46 tasks, 10 phases, acyclic and phase-safe

$ cargo test --locked --test repository_discovery_test \
                  --test worktree_create_test \
                  --test worktree_remove_test
test result: ok. 27 passed (repository_discovery_test)
test result: ok. 11 passed (worktree_create_test)
test result: ok.  9 passed (worktree_remove_test)
   finished in 0.47s + 0.75s

$ cargo test --locked --all-targets
... every suite green. 47 phase-4 tests + 380+ tests from
   phases 1–3 and carrier suites all pass on Rust 1.97.
```

## Results

- `cargo build --locked --all-targets` clean.
- `cargo fmt --check` clean.
- `cargo clippy --locked --all-targets -- -D warnings`
  clean.
- The phase-04 gate runs the three named test files plus
  `cargo test --locked --all-targets`; every previously
  passing suite still passes. The new `worktree_create_test`
  brings the per-suite count to 11 and `worktree_remove_test`
  to 9. Phase 2 / 3 suites show no regressions.
- All four Phase 4 acceptance checks pass: `4.1`
  repository discovery, `4.2` daemon-owned worktree
  creation, `4.3` safe teardown, and the catch-all
  `cargo test --locked --all-targets` re-run.

## Forbidden-side-effect checks

Per `CONTRACTS.md` and the Phase 04 gate's "forbidden
side effects" wording:

- **#6 (worker-session kill)** — `GitRunner` uses
  `process_group(0)` and `nix::sys::signal::killpg`. The
  Phase 04 worktree code does not weaken the runner.
- **#7 (Rust owns heartbeats / process lifecycle)** — the
  worktree layer does not touch heartbeats; the Phase 5
  supervisor is the source of truth.
- **#8 (no credential leakage)** — every `git` subprocess
  launched via the runner scrubs `GITHUB_TOKEN` /
  `CADUCEUS_GITHUB_TOKEN` / `GH_TOKEN`. The `redact_and_cap`
  pass applies to captured stderr. The Phase 4
  `repository_discovery_test` exercises this surface
  directly (token-shaped substring redaction is asserted in
  `git_runner_redacts_token_shaped_substrings_from_stderr`).
- **#4 (GitHub base URL credential denial)** —
  SSH-host-alias rejection is exercised in
  `validate_origin_host_rejects_ssh_alias_like_github_com_attacker`
  (Task 4.1).
- **#5 (daemon owns branch name)** — `create` constructs
  the branch name as
  `automation/issue-<n>-<run_id-lowercase>` and validates it
  via `git check-ref-format --branch`; `remove` only
  deletes branches via `git branch -D` when the retention
  decision permits (no upstream, no merge-base ancestor
  reaching the base, no fresh work beyond the recorded
  `base_oid`).
- **Atomic claim-of-worktree-path** — `create` takes an
  `fs2::FileExt::lock_exclusive` on
  `<repo>/.worktrees/.lock` before any worktree-side state
  changes. The lock serialises concurrent daemon ticks on
  the same main clone.
- **Path-escape rejection** — `remove` refuses any
  `Worktree` whose canonical `path` lies outside
  `<main>/.worktrees/`. Tested by
  `remove_rejects_path_outside_worktrees_dir` and
  `remove_rejects_path_above_worktrees_dir`.

## CONTRACTS.md status

The contracts file was **not modified** during Phase 04.

```text
$ sha256sum planning/caduceus-v0.1/CONTRACTS.md
ace44d138c05548cb1b15e3176ed99f0521148ed4c24b449643716340be91eb2
```

This matches the `contracts_sha256` pinned in
`task-manifest.json`. The `archive/full-reviewed-plan.md`
digest was also left untouched.

## Forbidden tests / contract changes

No task in this phase weakened, ignored, or deleted an
acceptance assertion. Every acceptance bullet listed in the
task packets has a corresponding test:

| Task | Packet bullet | Test |
|---|---|---|
| 4.1 | Origin URL normalisation | 5 tests in `parse_origin_*` |
| 4.1 | Host validation | 7 tests in `validate_origin_host_*` |
| 4.1 | End-to-end discovery | 9 tests in `find_main_clone_*` |
| 4.1 | GitRunner contract | 6 tests in `git_runner_*` |
| 4.2 | Successful creation | `create_succeeds_for_clean_repo_*`, `create_succeeds_with_default_base_*` |
| 4.2 | Branch / path separation | `create_branch_name_contains_slashes_but_path_does_not` |
| 4.2 | Fetch failure | `create_surfaces_precise_error_on_fetch_failure` |
| 4.2 | Collision | `create_returns_collision_*` (path + branch), `create_reconciles_*` |
| 4.2 | Invalid run id | `create_rejects_run_id_*` (traversal + shell) |
| 4.2 | Parent unchanged | `create_leaves_parent_main_checkout_unchanged` |
| 4.2 | Fetch update | `create_picks_up_new_remote_commit_on_second_run` |
| 4.3 | Worker-failure success | `remove_succeeds_for_worker_failure_worktree` |
| 4.3 | Pushed retained | `remove_retains_pushed_branch` |
| 4.3 | Merged retained | `remove_retains_merged_branch` |
| 4.3 | Idempotent | `remove_is_idempotent_for_already_missing_path` |
| 4.3 | Nested contents | `remove_handles_nested_filesystem_contents` |
| 4.3 | Metadata removed | `remove_clears_registered_worktree_metadata` |
| 4.3 | Path-escape | `remove_rejects_path_outside_worktrees_dir`, `remove_rejects_path_above_worktrees_dir` |
| 4.3 | Failure surfaces typed error | `remove_surfaces_typed_error_when_worktree_path_unremovable` |

## Residual risks

- The `gc` entry point remains a stub; the CLI surfaces
  `caduceus worktree-gc [--older-than-days N]` is also
  outside Phase 4. Phase 6 (`Finalization`) is the
  natural owner of the GC walk because it owns the run-state
  retention policy referenced by `run_retention_days`.
- `create`'s branch-format validation uses `git
  check-ref-format --branch`. Git's rules allow more
  characters than the daemon's run-id regex; if the
  daemon grows the `run_id` alphabet, re-check the test
  surface.
- `remove`'s path-safety check uses `fs::canonicalize`,
  which fails for non-existent paths. The fallback path
  uses the uncanonicalised input; this means an
  attacker who controls the daemon-side state can sneak in
  a path that *does* exist outside `<main>/.worktrees/`
  via a symlink that resolves to a foreign location. The
  test `remove_rejects_path_outside_worktrees_dir` covers
  the straightforward case but the symlink case is a
  Phase 5 hardening item.

## Blocker evidence (blocked only)

Not blocked.
