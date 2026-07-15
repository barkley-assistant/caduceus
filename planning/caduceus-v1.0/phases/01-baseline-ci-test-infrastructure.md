# Phase 01: Baseline CI and test infrastructure

## Intent

Establish required CI and hermetic fixtures before runtime implementation.

## Tasks

- [Task 1.1: Establish required CI matrix][task-1-1]
- [Task 1.2: Build hermetic GitHub and Git fixtures][task-1-2]
- [Task 1.3: Build process, crash, release-binary fixtures][task-1-3]
- [Task 1.4: Build the pinned Hermes host fixture][task-1-4]

[task-1-1]: ../tasks/1.1-establish-required-ci-matrix.md
[task-1-2]: ../tasks/1.2-build-hermetic-github-and-git-fixtures.md
[task-1-3]: ../tasks/1.3-build-process-crash-release-binary-fixtures.md
[task-1-4]: ../tasks/1.4-build-the-pinned-hermes-host-fixture.md

## Phase gate

- **PHASE-01-AC-01** — MSRV and stable CI pass the canonical gate.
- **PHASE-01-AC-02** — Fixture self-tests are hermetic.
- **PHASE-01-AC-03** — Commit validation requires the non-empty scope form
  `<type>(<scope>): <description>` from
  [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/).
  Type and scope are lowercase; the imperative description has no trailing
  period; and the complete subject is at most 80 characters. The exact valid
  example is `feat(lang): add Polish language example`.
- **PHASE-01-AC-04** — The pinned real-host lifecycle fixture is isolated and
  simulates present and absent tool capabilities.
- **PHASE-01-AC-05** — Baseline CI and fixture self-tests are green, and the
  installed-path walking-skeleton report records every command, exit, category,
  artifact, and owned expected gap.
- **PHASE-01-AC-06** — Fixture failures, unexpected or unowned failures, and
  silent success block; deterministic Phase 02 implementation gaps do not.

The gate handoff must map each phase acceptance ID to the exact command or
procedure, observed result, and durable artifact. The next phase does not
start until this gate is complete.

After this gate, plan refinement stops and implementation begins. Later
discoveries become evidence-backed issues or tasks, not speculative replanning.
