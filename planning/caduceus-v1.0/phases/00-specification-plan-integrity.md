# Phase 00: Specification and plan integrity

## Intent

Publish and seal a complete public task catalog and readiness audit. This phase
routes implementation gaps; it does not pretend they are fixed.

## Tasks

- [Task 0.1: Publish the v1.0 task catalog][task-0-1]
- [Task 0.2: Enforce acceptance evidence][task-0-2]
- [Task 0.3: Record the v0.1 MSRV historical deviation][task-0-3]
- [Task 0.4: Seal planning and archive integrity][task-0-4]

[task-0-1]: ../tasks/0.1-publish-the-v1-0-task-catalog.md
[task-0-2]: ../tasks/0.2-enforce-acceptance-evidence.md
[task-0-3]: ../tasks/0.3-record-the-v0-1-msrv-historical-deviation.md
[task-0-4]: ../tasks/0.4-seal-planning-and-archive-integrity.md

## Phase gate

- **PHASE-00-AC-01** — Catalog, evidence rules, contract digest, and v0.1 seal
  validate.
- **PHASE-00-AC-02** — All six readiness attachments exist, cross-link exactly,
  and cover the declared public surfaces and operator journeys.
- **PHASE-00-AC-03** — Every gap has exactly one task/acceptance owner or one
  approved deferral; no public promise remains unowned or contradicted.
- **PHASE-00-AC-04** — An independent contributor can reproduce every factual
  classification with the recorded commands.

The gate handoff must map each phase acceptance ID to the exact command or
procedure, observed result, and durable artifact. The next phase does not
start until this gate is complete.

## Relationship between phase gate and Task 0.1 evidence

The four phase acceptance IDs above are the **operator-facing summary**
of the gate. Task 0.1 produces the six readiness attachments and nine
acceptance IDs that roll up into those four phase checks: PHASE-00-AC-02
is satisfied by the six attachments produced under Task 0.1-AC-04 and
Task 0.1-AC-05, PHASE-00-AC-03 by the gap-register work in Task 0.1,
PHASE-00-AC-04 by the recorded commands in Tasks 0.1 and 0.2, and
PHASE-00-AC-01 by the validation tooling owned by Task 0.4 plus the
catalog published by Task 0.1. Treat the four IDs as the gate an
operator signs off; treat the nine Task 0.1 IDs as the evidence the
operator reads to do so.
