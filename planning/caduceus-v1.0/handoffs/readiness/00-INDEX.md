# Public Readiness Audit — Index

This directory contains the six cross-linked attachments required by
`CONTRACTS.md` `PLAN-005` and Phase 00 Task 0.1. Every claim made
here is grounded in code or documentation that exists in the
repository today; speculative or planned surfaces are labeled as
such and routed to a v1.0 task/acceptance ID.

## Attachments

1. [`01-capability-inventory.md`](./01-capability-inventory.md) —
   every public capability and its current state (`working-production`,
   `integrated-not-proven`, `stub`, `fake-only`, `planned`,
   `contradicted`). Satisfies **0.1-AC-01** and **0.1-AC-09**.
2. [`02-reachability-map.md`](./02-reachability-map.md) — every
   public CLI command and Hermes adapter path, walked through to its
   production function and test. Satisfies **0.1-AC-02**.
3. [`03-operator-journeys.md`](./03-operator-journeys.md) — the
   operator journey matrix (install, setup, cron, doctor, status,
   manual run, scheduled run, issue→PR, restart, merge/reject, update,
   migrate, recover, remove, uninstall). Satisfies **0.1-AC-03**.
4. [`04-fault-injection.md`](./04-fault-injection.md) — fault
   categories (Hermes tool errors, malformed/timeout/side-effect
   outcomes, configuration, GitHub, Git, worker, SQLite, OCI,
   gateway, permissions) and the production surfaces that catch
   them today. Satisfies **0.1-AC-04**.
5. [`05-requirement-evidence.md`](./05-requirement-evidence.md) —
   every contract requirement ID mapped to its acceptance ID and the
   current evidence state. Satisfies **0.1-AC-05**.
6. [`06-gap-register.md`](./06-gap-register.md) — every gap has
   exactly one existing task/acceptance owner or one approved
   deferral. Satisfies **0.1-AC-06**.

The cross-link and command-reproduction tests that satisfy
**0.1-AC-07** are described at the bottom of this index; the
synchronization check that satisfies **0.1-AC-08** is run by
`validate_plan.py` on every controller invocation.

## Reproduction commands

Every factual classification in this audit can be reproduced from a
clean working tree using the commands below. The commands do not
mutate the repository; they only read it.

```bash
# Plan validator (active catalog, contract digest, v0.1 seal, links)
python3 -B planning/caduceus-v1.0/tools/validate_plan.py

# Next-task selector (returns the first incomplete item)
python3 -B planning/caduceus-v1.0/tools/next_task.py --format json

# Inventory scan — every public surface listed in the README / docs /
# CLI / plugin / bridge / install / migration / recovery / release
# paths is reproduced in 01-capability-inventory.md.

# Contract digest match
sha256sum planning/caduceus-v1.0/CONTRACTS.md
grep contracts_sha256 planning/caduceus-v1.0/task-manifest.json

# v0.1 seal match
sha256sum planning/caduceus-v0.1/.progress.lock 2>/dev/null || true
python3 -c "
import hashlib, pathlib
root = pathlib.Path('planning/caduceus-v0.1').resolve()
digest = hashlib.sha256()
for path in sorted(root.rglob('*')):
    rel = path.relative_to(root).as_posix()
    if '__pycache__' in rel or rel.endswith('.pyc') or rel.endswith('.progress.lock'):
        continue
    if not path.is_file():
        continue
    digest.update(rel.encode()); digest.update(b'\0')
    digest.update(path.read_bytes()); digest.update(b'\0')
print(digest.hexdigest())
"
grep v01_tree_sha256 planning/caduceus-v1.0/task-manifest.json
```

## Capability state legend

| State | Meaning |
|---|---|
| `working-production` | Shipped v0.1 surface covered by a real production path and a real test that exercises the path. |
| `integrated-not-proven` | Shipped v0.1 surface; the wiring exists but a key behavior is missing, broken, or only exercised by helper tests. |
| `stub` | Shipped entry point that returns a hard-coded placeholder or an "implemented in task X.Y" error. |
| `fake-only` | The behavior is only exercised in tests using a fake harness (e.g. `tests/fake_ctx.py`, `wiremock`). |
| `planned` | Not shipped; planned by a v1.0 task with a clear acceptance ID. |
| `contradicted` | Shipped surface that disagrees with the operator docs or with `CONTRACTS.md`; the contradiction is recorded in `06-gap-register.md` with an owning task. |