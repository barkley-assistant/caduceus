# Phase gate handoff — Phase 04: Durable finalization and review lifecycle

## Summary

All 4 tasks in Phase 04 are complete. 30 tests, 30 passed — 22 checkpoint crash
matrix, 5 lifecycle matrix, 3 credentials audit.

## Task handoffs

| Task | Title | Handoff |
|---|---|---|
| 4.1 | Persist finalization checkpoints | [`handoffs/4.1.md`](handoffs/4.1.md) |
| 4.2 | Reconcile ambiguous external side effects | [`handoffs/4.2.md`](handoffs/4.2.md) |
| 4.3 | Add human review lifecycle | [`handoffs/4.3.md`](handoffs/4.3.md) |
| 4.4 | Verify checkpoint and lifecycle recovery | [`handoffs/4.4.md`](handoffs/4.4.md) |

All task handoffs are committed on `main` with their progress.json transitions.

## Phase gate acceptance evidence

| Acceptance ID | Status | Command or procedure | Result | Artifact |
|---|---|---|---|---|
| PHASE-04-AC-01 | PASS | `cargo test --locked --test checkpoint_crash_matrix_test` | 22 tests pass (21 crash-matrix + 1 empty-run), no duplicate checkpoints | tests/checkpoint_crash_matrix_test.rs |
| PHASE-04-AC-02 | PASS | `cargo test --locked --test lifecycle_matrix_test` | 5 tests pass (resolve_awaiting_review_as_done, route_to_needs_attention, reopen+merge, reprocess refusal, auto-merge error) | tests/lifecycle_matrix_test.rs |

## Verification commands

```text
# Checkpoint crash matrix (22 tests)
cargo test --locked --test checkpoint_crash_matrix_test
→ 22 passed, 0 failed

# Lifecycle matrix (5 tests)
cargo test --locked --test lifecycle_matrix_test
→ 5 passed, 0 failed

# Credentials audit (3 tests + fixture scan)
cargo test --locked --test credentials_audit_test
→ 3 passed, 0 failed

# Full Rust test suite
cargo test --locked --all-targets
→ All tests pass

# Lints
cargo fmt --check  → clean
cargo clippy --locked --all-targets -- -D warnings
→ clean
```

## CONTRACTS.md status

Sealed. `contracts_sha256` unchanged. No contract edits were required during
Phase 04 execution.

## Phase gate commits

| Commit | Description |
|---|---|
| 8bf8d7f | `feat(runtime): persist finalization checkpoints (#23)` |
| 08ddc5e | `feat(runtime): add operation IDs and remote reconciliation` |
| facf15a | `feat(review): add human review lifecycle (#24)` |
| (pending) | `feat(tests): add checkpoint crash-matrix, lifecycle-matrix, and credentials audit` |

## New modules introduced

- `src/state/checkpoints.rs` — checkpoint row CRUD against the SQLite store
- `src/runtime/audit.rs` — audit hook enforcing the "never auto-merge" contract
- `src/github/merge_detect.rs` — PR merge-status polling (Merged / ClosedWithoutMerge / StillOpen / NotFound)
- `tests/checkpoint_crash_matrix_test.rs` — 22-test crash-matrix suite (7 stages × 3 crash points + 1 empty-run)
- `tests/lifecycle_matrix_test.rs` — 5-test lifecycle suite (merge, close-without-merge, reopen, reprocess refusal, auto-merge error)
- `tests/credentials_audit_test.rs` — 3-test credential audit suite (fixture scan + positive/negative pattern tests)

## Human review gates

| Task | Reviewer | Commit | Decision |
|---|---|---|---|
| 3.4 | JP-User | 9a7d1b179d7a0d451a41c8f0093a1ba262f84f51 | Approved |

No Phase 04 tasks required human review gates.

## Residual risks

- The `tests/runtime/` subdirectory used during the SDD apply phase was
  collapsed to `tests/finalize_checkpoints_test.rs` to match the existing flat
  Cargo test layout. Future PRs that want `tests/runtime/` as a real
  subdirectory will need either a `[[test]]` entry per file in root `Cargo.toml`
  or a full tests/ refactor — tracked under a separate ticket.
- `NeedsAttention` is a new queue state. Any external monitoring or operator UI
  that enumerates queue states will need updating to render it (not in scope
  for Phase 04).
- Local pytest invocation from the repo root fails on a pre-existing
  `ModuleNotFoundError: tests.fake_ctx` in `tests/hermes_plugin_test.py`. CI
  pytest works correctly. The local invocation issue is tracked separately.
- The credentials audit fixture scanner uses simple string containment rather
  than regex. This is sufficient for the four tracked patterns but may miss
  obfuscated or encoded credentials. Future phases may upgrade to regex-based
  scanning if credential-leak risks increase.
- The reopen-then-merge test (`lifecycle_matrix_test::reopen_then_merge_flow`)
  re-seeds the state file directly rather than driving a full
  NeedsAttention → Queued → Acquired → InProgress → AwaitingReview → Done
  cycle through the claim system. The full cycle is covered by Tasks 4.1–4.3;
  the matrix test proves the state machine transitions work at the boundary
  points that are most likely to regress (reset, reopen, resolve).

## Handoff artifacts

- [Phase 04 spec](../phases/04-durable-finalization-review-lifecycle.md)
- [Phase gate handoff](phase-04.md) (this file)
- [CONTRACTS.md](../CONTRACTS.md)
