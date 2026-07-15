# Caduceus v1.0 Execution Plan

This directory is the public planning surface for Caduceus v1.0, the release
after the v0.1 implementation already shipped. Each file under
[tasks/](./tasks/) is a standalone GitHub issue or SDD work unit. The files
under [phases/](./phases/) group those units into a safe implementation order.

## Authority

1. [CONTRACTS.md](./CONTRACTS.md) is the single normative v1.0
   specification and owns all cross-task invariants.
2. The current file under [tasks/](./tasks/) owns task-local behavior and
   acceptance criteria.
3. [task-manifest.json](./task-manifest.json) provides machine-readable task
   IDs, dependencies, phases, ownership, tests, and acceptance mappings.
4. [phases/](./phases/) owns phase entry and exit gates.
5. [progress.json](./progress.json) records execution state only; it
   never changes requirements.
6. [CONTRACT_REVISIONS.md](./CONTRACT_REVISIONS.md) records authorized
   revisions to the sealed v1.0 contract.

If a task conflicts with `CONTRACTS.md`, stop and report the conflict.
Do not silently redesign a contract.

## Relationship to v0.1

V0.1 is shipped and immutable. Its plan, contracts, tasks, handoffs,
and controller record remain at `planning/caduceus-v0.1/`. V1.0 uses
that tree only as implementation evidence and historical context. A
v1.0 correction to a shipped behavior is specified here; it never
rewrites the archive.

The v1.0 contract intentionally restates every inherited public surface
needed by the release. Implementers do not need to choose between two
active contracts.

## Contract mismatch

A `contracts_sha256` mismatch is a safety stop. An implementation agent
must not repair it by editing the digest. Record the contradiction as a
bounded blocker and await an explicitly authorized contract revision.
The reviewer records the revision, updates all affected v1.0 planning
surfaces, refreshes the digest, and runs the validator. The archived
v0.1 plan and digest are never refreshed.

## Optional local sequencing

Contributors may use [AGENT_LOOP.md](./AGENT_LOOP.md) and the scripts under
`tools/` to select and record one local work item at a time. GitHub issues and
the task packets remain the primary public workflow; the local ledger does not
change requirements.

The optional loop is:

1. Run `python3 -B planning/caduceus-v1.0/tools/validate_plan.py`.
2. Run `python3 -B planning/caduceus-v1.0/tools/next_task.py
   --format json`.
3. Claim the returned item with `set_status.py`.
4. Load the contract, phase, task packet, and dependency handoffs.
5. Implement only the selected scope and preserve unrelated changes.
6. Run every acceptance check and the appropriate repository gate.
7. Write the task handoff with one-to-one acceptance evidence.
8. Complete the item through `set_status.py`; required human review
   keeps work `in_progress` until its artifact exists.
9. Start a fresh context for the next item.

When the selector returns `kind: blocked` or `kind: done`, stop and
report it. Do not bypass the controller.

## Phase order

- **Phase 00 — Specification and plan integrity.** Seal the contract,
  make evidence enforceable, and complete the detailed catalog without
  modifying the v0.1 archive.
- **Phase 01 — Baseline CI and test infrastructure.** Put the canonical
  MSRV, stable, Python, and planning gates in GitHub Actions and build
  the reusable system fixtures, including the pinned real Hermes host.
- **Phase 02 — Worker and Git runtime correctness.** Fix production
  configuration and transactional scheduling first, then repair the worker
  path, process supervision, transcripts, Git runner, and status exit codes.
- **Phase 03 — SQLite migration and recovery.** Make SQLite the v1.0
  runtime store and provide safe, explicit migration and recovery.
- **Phase 04 — Durable finalization and review lifecycle.** Add durable
  checkpoints, `AwaitingReview`, `NeedsAttention`, and human-merge
  reconciliation.
- **Phase 05 — Scheduling, repositories, and throughput.** Add fenced
  bounded concurrency, circuit breakers, daemon-owned mirrors, and
  explicit repository scope.
- **Phase 06 — Isolated execution.** Add the default OCI executor and
  explicit trusted-host compatibility mode.
- **Phase 07 — Full-system verification and release.** Run the complete
  regression suite, real Hermes lifecycle, release-binary canary, and
  maintainer release handoff.

The detailed catalog must preserve four v0.1 carryovers: the status
exit-code correction belongs in Phase 02; backup retention and the
shared `install::atomic_write` foundation belong in Phase 03; and the
stale MSRV packet is recorded in Phase 00 as history without changing
the v0.1 archive. Each becomes an issue-ready task with stable
acceptance IDs when the full catalog is written. Phase 00 does not claim
to implement the three runtime and state items.

Phase 00 publishes the readiness audit and routes every observed gap. Phase 01
runs the early installed-path walking skeleton. After its gate, implementation
begins; later discoveries become evidence-backed issues or tasks rather than
speculative replanning.

GitHub Apps, webhooks, auto-merge, multi-host coordination, custom
bridge composition, dashboards, and automated release tooling are
deferred to v1.x.
