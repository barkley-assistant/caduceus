# Phase gate handoff — Phase 05: Scheduling, repositories, and throughput

## Summary

All 5 tasks in Phase 05 are complete. 1015 tests across 74 suites pass locally
and the plan validator reports the catalog is acyclic and phase-safe. Phase 05
introduced safe single-host throughput (leadership, leases, concurrency pool,
exclusion, drain, backpressure, circuit breakers) and moved repositories fully
into daemon-owned storage. With Phase 05 sealed, the controller selects the
Phase 06 gate and the executor surface (Phase 6.1) becomes the next work item.

## Task handoffs

| Task | Title | Handoff |
|---|---|---|
| 5.1 | Add scheduler leadership and fenced leases | [`handoffs/5.1.md`](handoffs/5.1.md) |
| 5.2 | Add bounded concurrency and repository exclusion | [`handoffs/5.2.md`](handoffs/5.2.md) |
| 5.3 | Bound infrastructure failures | [`handoffs/5.3.md`](handoffs/5.3.md) |
| 5.4 | Move repositories into daemon storage | [`handoffs/5.4.md`](handoffs/5.4.md) |
| 5.5 | Scope and increment GitHub discovery | [`handoffs/5.5.md`](handoffs/5.5.md) |

All task handoffs are committed on `main` with their progress.json transitions.

## Phase gate acceptance evidence

| Acceptance ID | Status | Command or procedure | Result | Artifact |
|---|---|---|---|---|
| PHASE-05-AC-01 | PASS | `cargo test --locked --test scheduler_leadership_test ; cargo test --locked --test scheduler_leases_test ; cargo test --locked --test scheduler_pool_test ; cargo test --locked --test daemon_drain_test` | 5 leadership tests + 7 lease tests + 6 pool tests + 4 drain tests pass: two-thread contention yields exactly one winner; stale fencing tokens rejected; same repo serializes while distinct repos concurrent; `PoolSaturated` returned on tight budget; drain stops admission and returns `DrainTimeout` | handoffs/5.1.md, handoffs/5.2.md |
| PHASE-05-AC-02 | PASS | `cargo test --locked --test scheduler_circuit_test ; cargo test --locked --test scheduler_circuit_dispatch_test ; cargo test --locked --test failure_separation_test` | 5 circuit tests + 5 dispatch tests + 4 failure-separation tests pass: 3-failure threshold opens, 30-minute half-open interval, recovery on probe, 24-hour escalation to NeedsAttention, worker and infrastructure counters stay separate | handoffs/5.3.md |
| PHASE-05-AC-03 | PASS | `cargo test --locked --test mirror_test ; cargo test --locked --test storage_root_test ; cargo test --locked --test worktree_test ; cargo test --locked --test startup_test ; cargo test --locked --test discovery_pagination_test` | 4 mirror tests + 5 storage-root tests + 3 worktree tests + 4 startup tests + 7 discovery-pagination tests pass: bare mirrors created at storage path with mode 0700; symlinked storage root rejected; daemon creates its own worktrees; bounded incremental pagination through `/user/repos` with `discovery_max_pages` cap | handoffs/5.4.md, handoffs/5.5.md |

## Verification commands

```text
# Scheduler leadership + leases (Task 5.1)
cargo test --locked --test scheduler_leadership_test
→ 5 passed, 0 failed
cargo test --locked --test scheduler_leases_test
→ 7 passed, 0 failed

# Concurrency pool + exclusion + drain + backpressure (Task 5.2)
cargo test --locked --test scheduler_pool_test
→ 6 passed, 0 failed
cargo test --locked --test daemon_drain_test
→ 4 passed, 0 failed

# Circuit breakers (Task 5.3)
cargo test --locked --test scheduler_circuit_test
→ 5 passed, 0 failed
cargo test --locked --test scheduler_circuit_dispatch_test
→ 5 passed, 0 failed
cargo test --locked --test failure_separation_test
→ 4 passed, 0 failed

# Daemon-owned storage (Task 5.4)
cargo test --locked --test mirror_test
→ 4 passed, 0 failed
cargo test --locked --test storage_root_test
→ 5 passed, 0 failed
cargo test --locked --test worktree_test
→ 3 passed, 0 failed
cargo test --locked --test startup_test
→ 4 passed, 0 failed

# Bounded incremental discovery (Task 5.5)
cargo test --locked --test discovery_pagination_test
→ 7 passed, 0 failed
cargo test --locked --test audit_redaction_test
→ 7 passed, 0 failed
cargo test --locked --test api_base_allowlist_test
→ 14 passed, 0 failed

# Full Rust test suite
cargo test --locked --all-targets
→ 1015 passed (74 suites)

# Plan validator
python3 planning/caduceus-v1.0/tools/validate_plan.py
→ plan valid (active catalog): 42 tasks, 8 phases, acyclic and phase-safe

# Lints
cargo fmt --check  → clean
cargo build --locked --all-targets  → 0 errors, 9 warnings (all pre-existing
                                       `[[test]] edition` deprecation notices
                                       from subdirectory tests; informational)
```

## CONTRACTS.md status

Sealed. `contracts_sha256` unchanged. No contract edits were required during
Phase 05 execution.

## Phase gate commits

| Commit | Description |
|---|---|
| `1b066a5` | `feat(scheduler): add scheduler leadership and fenced leases (#30)` |
| `59348b4` | `feat(scheduler): add bounded concurrency pool and repository exclusion (#26)` |
| `6f0e809` | `feat(scheduler): add circuit breaker infrastructure for failure control (#32)` |
| `04403e4` | `feat(repo): add daemon-owned bare mirrors and disposable worktrees (#33)` |
| `dd6e410` | `feat(github): scope and increment GitHub discovery (#29)` |

Plus the five `docs(plan): mark Task 5.X complete in controller` flips and
the human-review sign-off commit (`405ec6b`) that sealed Task 5.1's review
artifact.

## New modules introduced

- `src/scheduler/leadership.rs` — `LeaderToken` with `try_acquire` and `with_lock`
- `src/scheduler/leases.rs` — fenced leases with monotonic fencing tokens
- `src/scheduler/pool.rs` — bounded concurrency pool with `DrainConfig` and `PoolState`
- `src/scheduler/exclusion.rs` — `RepoExclusionMap` (per-repository serialization)
- `src/scheduler/circuit.rs` — `CircuitStore`, `CircuitState`, `CircuitScope`, backoff, admission
- `src/infra/error.rs` — phase-5 variants (`SymlinkedStorageRoot`, `WorktreeReuseAfterFailure`, `ModeNotPreserved`, `PoolSaturated`, `DrainTimeout`, `FencingTokenRegression`, `LeaseStale`, `ApiBaseNotAllowed`, `EtagMismatch`, `PaginationExhausted`)
- `src/infra/config.rs` — `repo_storage_root`, `discovery_max_pages`, `validate_api_base`, `repo_scope`, `api_base` fields (validated at `Config::load`)
- `src/github/api_base.rs` — positive allowlist validator (GitHub.com SaaS + GHES host pattern)
- `src/github/discovery.rs` — per-scope polling with ETag-aware incremental fetch and pagination
- `src/runtime/audit.rs` — discovery audit events with `redact_pat` step
- `tests/scheduler_leadership_test.rs` — 5 tests
- `tests/scheduler_leases_test.rs` — 7 tests
- `tests/scheduler/pool_test.rs` — 6 tests
- `tests/scheduler/circuit_test.rs` — 5 tests
- `tests/scheduler/circuit_dispatch_test.rs` — 5 tests
- `tests/scheduler/failure_separation_test.rs` — 4 tests
- `tests/daemon/drain_test.rs` — 4 tests
- `tests/repo/mirror_test.rs` — 4 tests
- `tests/repo/storage_root_test.rs` — 5 tests
- `tests/repo/worktree_test.rs` — 3 tests
- `tests/daemon/startup_test.rs` — 4 tests
- `tests/discovery_pagination_test.rs` — 7 tests
- `tests/audit_redaction_test.rs` — 7 tests
- `tests/api_base_allowlist_test.rs` — 14 tests

## Human review gates

No Phase 05 tasks required human review per the gap register. Task 5.1's
PR review was approved by JP-User (commit `405ec6b`) but the gap-register
"Human review required?" column for row G-22 (which Task 5.5 partially
implements) is No. The review-required surface begins at Phase 6 with
Task 6.4 (`human-review` label applied at ticket creation).

## Residual risks

- Local pytest invocation from the repo root fails with a pre-existing
  `ModuleNotFoundError: tests.fake_ctx` (the same issue phase-04 documented).
  CI pytest is green. The local invocation issue is tracked separately and
  not introduced by Phase 05.
- The 9 `cargo build` warnings are all pre-existing `[[test]] edition`
  deprecation notices from subdirectory test targets — informational,
  introduced across Phases 02–05. The fix is a future cargo-version upgrade.
- Phase 05 inherits the scheduler-parallelism test pattern from Task 5.2
  (distinct keys for parallelism tests, same key for exclusion tests) — that
  rule is encoded in the `sdd-runbook-caduceus-phase-5-and-beyond.md`
  reference for Phase 6+ work.
- Task 5.5's `validate_api_base` accepts `http://localhost` and `http://127.0.0.1`
  for test-only mock-server compatibility (loopback is not routable; production
  security unaffected). Future phases may want to gate this behind a `cfg(test)`
  flag rather than a runtime branch.

## Handoff artifacts

- [Phase 05 spec](../phases/05-scheduling-repositories-throughput.md)
- [Phase gate handoff](phase-05.md) (this file)
- [CONTRACTS.md](../CONTRACTS.md)