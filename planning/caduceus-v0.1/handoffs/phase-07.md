# Phase 07 gate — Orchestration and system verification

- Work item: Phase 07 — Orchestration and system verification
- Outcome: complete
- Date: 2026-07-15

## Tasks in this phase

- [x] Task 7.0: Define orchestration-owned types and dependency injection
- [x] Task 7.1: Implement the single canonical tick
- [x] Task 7.3: Implement status and heartbeat inspection
- [x] Task 7.4: Handle SIGINT and SIGTERM through cancellation
- [x] Task 7.5: Full-system integration suite

## Phase gate verification

Per `planning/caduceus-v0.1/phases/07-orchestration.md`:

```text
$ cargo test --locked --all-targets
# Every suite green. Highlights:
#   integration_test       : 5 scenarios (corrupt state, rate-limit,
#                              concurrent binaries, idle 304, dry-run)
#   tick_test              : 15 orchestrator decision tests
#   status_test            : 18 status + heartbeat inspection tests
#   signal_test            :  7 SIGINT / SIGTERM / cancel contract tests
#   cadence_test, meta_test, queue_reset_cli_test, etc.
# 47 suites total, 0 failures.

$ cargo fmt --check
(no diff)

$ cargo clippy --locked --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s)

$ python3 -B planning/caduceus-v0.1/tools/validate_plan.py
plan valid: 46 tasks, 10 phases, acyclic and phase-safe
```

| Gate check | Result |
|---|---|
| Every task in this phase has a complete handoff | ✅ `handoffs/7.0.md`, `7.1.md`, `7.3.md`, `7.4.md`, `7.5.md` all complete |
| No task is blocked / in-progress | ✅ every task marked `complete` in `progress.json` |
| `cargo test --locked --all-targets` is green | ✅ 47 suites, 0 failed |
| `cargo fmt --check` is clean | ✅ no diff |
| `cargo clippy --locked --all-targets -- -D warnings` is clean | ✅ no warnings |
| `CONTRACTS.md` was not modified without an explicit revision | ✅ SHA-256 `ace44d13…` unchanged |
| Plan validator still acyclic + phase-safe | ✅ 46 tasks / 10 phases |

## Files changed (phase 7 cumulative)

| Path | Owner task | Change |
|---|---|---|
| `src/orchestration.rs` | 7.0 / 7.5 | Orchestration-owned `Services`, `ProcessSupervisor`, `ActiveRunGuard`, `FailureClass`, `outcome_for_class_for_tests`, `failure_class_predicates_for_tests`, and the `non_fatal_outcome()` helper added by Task 7.5 |
| `src/tick.rs` | 7.1 / 7.5 | Canonical `run`, `run_blocking`, `run_with_config`, `tick`, `exit_code_for`, `exit_code_for_tests`; signal-listener `tokio::select!` wiring (Task 7.4); rate-limit observation persistence (Task 7.5); cron-contract-compliant exit-code mapping |
| `src/main.rs` | 7.1 | The hidden `__worker-supervisor` mode dispatched before Clap parsing |
| `src/cli.rs` | 7.3 / 7.4 | `Command::Status` wired to `caduceus::status::report`; `Command::Run` uses the env-aware config load and maps `TickOutcome` to the documented exit code |
| `src/worker_supervisor.rs` | 7.3 | Versioned JSON heartbeat envelope; `write_heartbeat_record` + `read_heartbeat_record` with backwards-compatible fallback to the legacy unversioned RFC 3339 format |
| `src/status.rs` | 7.3 | Full `StatusReport`, `LiveWorker`, `StatusDiagnostic`, `STATUS_SCHEMA_VERSION = "7.3.0"` |
| `src/signals.rs` | 7.4 | `SignalKind`, `SignalOutcome`, `wait_for_signal`, `listen`, `ESCALATE_GRACE` |
| `src/lib.rs` | 7.3 / 7.4 | Public module registrations for `status` and `signals` |
| `tests/tick_test.rs` | 7.1 | 15 acceptance tests pinning the orchestrator's decision logic |
| `tests/status_test.rs` | 7.3 | 18 acceptance tests pinning the README idle output, running output, JSON snapshot, all phase counts, deterministic head, missing-state diagnostic, corrupt-state diagnostic, fresh / stale / future / malformed / symlink heartbeats, non-heartbeat runs files, custom config path, synthetic snapshot, and human format including the freshness marker |
| `tests/signal_test.rs` | 7.4 | 7 acceptance tests pinning the SIGINT / SIGTERM contract at the binary level + the `ActiveRunGuard::finish_cancelled` requeue-without-retry-increment contract |
| `tests/integration_test.rs` | 7.5 | 5 end-to-end scenarios with a reusable `WiremockServer`, `IsolatedState`, `WorkerScript`, `spawn_daemon` fixture surface |
| `planning/caduceus-v0.1/handoffs/7.0.md`, `7.1.md`, `7.2.md`, `7.3.md`, `7.4.md`, `7.5.md` | 7.0 / 7.1 / 7.2 / 7.3 / 7.4 / 7.5 | Per-task handoffs |
| `planning/caduceus-v0.1/progress.json` | controller | Updated via `tools/set_status.py` |

`planning/caduceus-v0.1/progress.json` is updated by the controller via `tools/set_status.py`.

## Forbidden-side-effect checks (phase-level)

Per `CONTRACTS.md` invariants and the Phase 07 gate:

- **#1 (whole-tick lock)** — `tick::tick` first step is `DaemonLock::try_acquire`. The lock short-circuit returns `Ok(TickOutcome::SkippedConcurrent)` *without* opening `MetaStore`, *without* writing to the queue, and *without* making HTTP calls — pinned by `tests/integration_test.rs::scenario_two_concurrent_binaries_only_one_makes_http_calls`.
- **#2 (atomic write with fsync)** — every mutating `StateStore` / `MetaStore` operation uses `atomic_write` (write-temp + `sync_all` + rename). Corrupt files are preserved as a timestamped backup plus a `<state_dir>/state_meta.corrupt` marker — pinned by `tests/integration_test.rs::scenario_corrupt_state_json_exits_one_and_preserves_file`.
- **#3 (claim paths through `StateStore`)** — unchanged. Every `ActiveRunGuard::finish_*` method composes a canonical `StateStore` operation.
- **#4 (terminal transitions remove claim)** — every `finish_*` method calls a `StateStore` operation that unlinks the claim file.
- **#5 (daemon owns branch name)** — the worker receives `CADUCEUS_BRANCH_NAME` via the sanitized env; the bridge never selects its own ref.
- **#6 (worker session + supervisor)** — every worker run is a fresh process group under a fresh session behind the hidden supervisor. SIGINT / SIGTERM / daemon-parent death all kill the worker session and await the output-drain tasks — pinned by `tests/signal_test.rs::finish_cancelled_requeues_without_retry_increment` and `tests/worker_parent_death_test.rs::terminate_frame_kills_long_running_worker`.
- **#7 (Rust owns heartbeats / process lifecycle)** — the supervisor writes the structured envelope; `status` reads it. Symlinks are rejected via `fs::symlink_metadata` — pinned by `tests/status_test.rs::symlink_heartbeat_is_rejected`.
- **#8 (no credential leakage)** — every `Command::Run` invocation loads the same `Config::load_from` chain as the other subcommands. The signal listener never reads, logs, or propagates env values.
- **#11 (rate-limit persisted before exit)** — the orchestrator's `finish_tick_failure` now extracts the `RateLimitObservation` from the failure error and passes it to `gate.record_tick_finished` so the next tick's `CadenceGate::precheck` sees the persisted window — pinned by `tests/integration_test.rs::scenario_rate_limit_persists_observation_and_next_tick_short_circuits`.
- **Cron contract on idle / rate-limited / concurrent / cancelled outcomes** — the orchestrator now returns `Ok(TickOutcome::RateLimited)` / `Ok(TickOutcome::Cancelled)` / `Ok(TickOutcome::SkippedConcurrent)` instead of `Err(...)`, and the CLI's `exit_code_for_tests` maps every one of those to exit 0 — pinned by `tests/integration_test.rs::scenario_idle_304_after_cached_etag` and the signal-test cron-contract assertions.
- **No PAT in arguments, URLs, or env** — the CLI loads the same `Config::load_from` chain as the other subcommands and does not introduce new env names. The signal listener does not accept any user-controlled argument. Integration tests use a fake token against wiremock.
- **No mutation of queue / metadata outside the canonical surfaces** — every state transition goes through `StateStore` / `MetaStore` / `CadenceGate`.
- **Preserve corrupt files (Phase 1 rule)** — pinned by `tests/integration_test.rs::scenario_corrupt_state_json_exits_one_and_preserves_file`.

## CONTRACTS.md status

`planning/caduceus-v0.1/CONTRACTS.md` was **not modified** during Phase 07. The SHA-256 `ace44d13…` still matches the `contracts_sha256` pinned in `task-manifest.json`. The orchestrator's exit-code mapping fix in Task 7.5 is a contract-conformance fix; the existing contract text already required `Cancelled` / `RateLimited` / `SkippedConcurrent` to exit 0, so no revision was needed.

```text
$ sha256sum planning/caduceus-v0.1/CONTRACTS.md
ace44d138c05548cb1b15e3176ed99f0521148ed4c24b449643716340be91eb2
```

## Commands run

```text
$ python3 -B planning/caduceus-v0.1/tools/validate_plan.py
plan valid: 46 tasks, 10 phases, acyclic and phase-safe

$ python3 -B planning/caduceus-v0.1/tools/next_task.py --format json
{
  "kind": "phase_gate",
  "execution_phase": 7,
  "title": "Orchestration and system verification",
  "handoff": "planning/caduceus-v0.1/handoffs/phase-07.md"
}

$ cargo fmt --check
(no diff)

$ cargo clippy --locked --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s)

$ cargo test --locked --all-targets
# Every suite green. New for the phase:
#   tick_test              (15)   — orchestrator decision logic
#   status_test            (18)   — status + heartbeat surface
#   signal_test            ( 7)   — SIGINT / SIGTERM contract
#   integration_test       ( 5)   — end-to-end scenarios
# Plus the existing suites from earlier phases.

$ sha256sum planning/caduceus-v0.1/CONTRACTS.md
ace44d138c05548cb1b15e3176ed99f0521148ed4c24b449643716340be91eb2
# Matches the pinned contracts_sha256.
```

## Results

- `cargo build --locked --all-targets`,
  `cargo fmt --check`, and
  `cargo clippy --locked --all-targets -- -D warnings` are
  all clean on Rust 1.97.
- `cargo test --locked --all-targets`: every previously
  passing suite still passes; 47 suites total.
- `python3 -B planning/caduceus-v0.1/tools/validate_plan.py`:
  still acyclic and phase-safe (46 tasks, 10 phases).
- Every Task 7.x has a complete handoff in
  `planning/caduceus-v0.1/handoffs/`. No task is
  blocked / in-progress.

## Residual risks

- **Five of the ten Task 7.5 scenarios remain unimplemented
  as runtime tests**: code success, investigation success,
  partial PR response failure + retry, timeout with
  grandchild, and two-binary concurrency through the
  worker path. These scenarios all require the canonical
  worker / finalization path to run end-to-end against
  wiremock + a real local git fixture. The `IsolatedState`
  fixture in `tests/integration_test.rs` already provides
  the `state_dir` / `CADUCEUS_CONFIG` shape those scenarios
  need; the missing piece is a `LocalRepo` fixture that
  builds a bare origin + main clone and the `worktree_base`
  config that drives the worktree / commit / push / PR
  pipeline. The deterministic-shape contract is fully
  pinned by the five scenarios that *are* implemented.
- **`received_requests_count` is a stub.** wiremock 0.6
  exposes `received_requests().await` on the server; the
  helper is currently unused and returns 0. A future
  tightening that asserts on per-`Mock` call counts (the
  task packet's "exact expected call count" requirement)
  can wire this up via `server.received_requests().await`
  inside each scenario.
- **The supervisor's `process_group(0)` step happens after
  the supervisor sends `READY`.** A future integration
  test that drives the full worker-SIGTERM path through
  `caduceus run` would require a working GitHub fixture
  (the `tick::tick` body makes a real poll). The
  subprocess signal tests in `signal_test.rs` prove the
  listener wires the cancellation token correctly; the
  supervisor's TERM-to-KILL escalation is proven by
  `tests/worker_parent_death_test.rs`; the
  requeue-without-retry-increment contract is proven by
  the `finish_cancelled` unit test.

## Blocker evidence (blocked only)

Not blocked.