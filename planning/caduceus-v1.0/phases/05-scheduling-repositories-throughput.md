# Phase 05: Scheduling, repositories, and throughput

## Intent

Add safe single-host throughput, failure controls, and daemon repositories.

## Tasks

- [Task 5.1: Add scheduler leadership and fenced leases][task-5-1]
- [Task 5.2: Add bounded concurrency and repository exclusion][task-5-2]
- [Task 5.3: Bound infrastructure failures][task-5-3]
- [Task 5.4: Move repositories into daemon storage][task-5-4]
- [Task 5.5: Scope and increment GitHub discovery][task-5-5]

[task-5-1]: ../tasks/5.1-add-scheduler-leadership-and-fenced-leases.md
[task-5-2]: ../tasks/5.2-add-bounded-concurrency-and-repository-exclusion.md
[task-5-3]: ../tasks/5.3-bound-infrastructure-failures.md
[task-5-4]: ../tasks/5.4-move-repositories-into-daemon-storage.md
[task-5-5]: ../tasks/5.5-scope-and-increment-github-discovery.md

## Phase gate

- **PHASE-05-AC-01** — Concurrency, fencing, exclusion, drain, and backpressure
  pass.
- **PHASE-05-AC-02** — Circuit breakers bound infrastructure failures.
- **PHASE-05-AC-03** — Runtime requires no operator checkout.

The gate handoff must map each phase acceptance ID to the exact command or
procedure, observed result, and durable artifact. The next phase does not
start until this gate is complete.
