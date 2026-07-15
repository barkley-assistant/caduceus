# Phase 07: Full-system verification and release

## Intent

Prove the release through production-path integration and public evidence.

## Tasks

- [Task 7.1: Complete ten canonical integration scenarios][task-7-1]
- [Task 7.2: Run v1 cross-subsystem failure matrix][task-7-2]
- [Task 7.3: Verify real Hermes lifecycle][task-7-3]
- [Task 7.4: Publish v1 operator documentation][task-7-4]
- [Task 7.5: Run release-binary canary][task-7-5]
- [Task 7.6: Complete release readiness][task-7-6]

[task-7-1]: ../tasks/7.1-complete-ten-canonical-integration-scenarios.md
[task-7-2]: ../tasks/7.2-run-v1-cross-subsystem-failure-matrix.md
[task-7-3]: ../tasks/7.3-verify-real-hermes-lifecycle.md
[task-7-4]: ../tasks/7.4-publish-v1-operator-documentation.md
[task-7-5]: ../tasks/7.5-run-release-binary-canary.md
[task-7-6]: ../tasks/7.6-complete-release-readiness.md

## Phase gate

- **PHASE-07-AC-01** — All ten scenarios pass with exact mutation counts.
- **PHASE-07-AC-02** — The failure matrix and real Hermes lifecycle pass.
- **PHASE-07-AC-03** — The release canary has independent approval.
- **PHASE-07-AC-04** — CI, docs, versions, evidence, and v0.1 integrity pass.
- **PHASE-07-AC-05** — The recorded candidate commit/archive and binary SHA-256
  pass installed setup, scheduling, doctor, and status.
- **PHASE-07-AC-06** — Manual dry run has zero mutations; scheduled non-dry run
  has the exact contracted Git and GitHub mutation counts.
- **PHASE-07-AC-07** — Request log, object IDs, run ID, PR URL,
  transcript/report, cleanup, and independent approval are complete before any
  readiness claim.

The gate handoff must map each phase acceptance ID to the exact command or
procedure, observed result, and durable artifact. The next phase does not
start until this gate is complete.
