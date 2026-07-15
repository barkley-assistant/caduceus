# Phase 06: Isolated execution

## Intent

Provide OCI isolation while retaining an explicit trusted-host mode.

## Tasks

- [Task 6.1: Introduce executor interface][task-6-1]
- [Task 6.2: Implement OCI CLI executor][task-6-2]
- [Task 6.3: Enforce isolation policy][task-6-3]
- [Task 6.4: Verify isolation boundary][task-6-4]

[task-6-1]: ../tasks/6.1-introduce-executor-interface.md
[task-6-2]: ../tasks/6.2-implement-oci-cli-executor.md
[task-6-3]: ../tasks/6.3-enforce-isolation-policy.md
[task-6-4]: ../tasks/6.4-verify-isolation-boundary.md

## Phase gate

- **PHASE-06-AC-01** — OCI lifecycle and result handling pass.
- **PHASE-06-AC-02** — Isolation review is approved.
- **PHASE-06-AC-03** — Git-less execution, secret cleanup, baseline policy, and
  orphan reconciliation pass under Docker and Podman-compatible CLIs.

The gate handoff must map each phase acceptance ID to the exact command or
procedure, observed result, and durable artifact. The next phase does not
start until this gate is complete.
