# Phase 02: Worker and Git runtime correctness

## Intent

Make installed Hermes configuration and scheduling truthful before correcting
the production worker, process, transcript, Git, and status paths.

## Tasks

- [Task 2.1: Implement production configuration bootstrap][task-2-1]
- [Task 2.2: Make Hermes scheduling transactional and diagnosable][task-2-2]
- [Task 2.3: Unify production worker execution][task-2-3]
- [Task 2.4: Enforce worker deadlines and process-tree cleanup][task-2-4]
- [Task 2.5: Bound and report worker transcripts][task-2-5]
- [Task 2.6: Harden every Git invocation][task-2-6]
- [Task 2.7: Correct status command exit codes][task-2-7]
- [Task 2.8: Prove the corrected runtime path][task-2-8]

[task-2-1]: ../tasks/2.1-implement-production-configuration-bootstrap.md
[task-2-2]: ../tasks/2.2-make-hermes-scheduling-transactional-and-diagnosable.md
[task-2-3]: ../tasks/2.3-unify-production-worker-execution.md
[task-2-4]: ../tasks/2.4-enforce-worker-deadlines-and-process-tree-cleanup.md
[task-2-5]: ../tasks/2.5-bound-and-report-worker-transcripts.md
[task-2-6]: ../tasks/2.6-harden-every-git-invocation.md
[task-2-7]: ../tasks/2.7-correct-status-command-exit-codes.md
[task-2-8]: ../tasks/2.8-prove-the-corrected-runtime-path.md

## Phase gate

- **PHASE-02-AC-01** — Setup, status, and run load production configuration.
- **PHASE-02-AC-02** — Cron lifecycle rollback and structured doctor pass in
  present and absent host-capability fixtures.
- **PHASE-02-AC-03** — The real bridge completes through production.
- **PHASE-02-AC-04** — Worker lifecycle review is approved.
- **PHASE-02-AC-05** — Git adversarial and status tests pass.
- **PHASE-02-AC-06** — The production-surface scanner and installed
  hook/script lifecycle find no stub, pending-work, fake-only, development-only,
  inaccurate comment, unsafe generated-script, or unactionable-error surface.

The gate handoff must map each phase acceptance ID to the exact command or
procedure, observed result, and durable artifact. The next phase does not start
until this gate is complete.
