# Handoff template

Copy this file to `<task-id>.md` or `phase-XX.md`.

- Work item:
- Outcome: complete | blocked
- Files changed:
- Public signatures/contracts used:
- State/schema effects:
- Tests added or changed:
- Commands run:
- Results:
- Forbidden-side-effect checks:
- Residual risks:
- Blocker evidence (blocked only):

## Acceptance evidence

Use one row for every `acceptance_ids` entry in the manifest. The
validator accepts only `PASS` or `PASSED` in the status column for a
completed item. Command or procedure, observed result, and durable
artifact or test reference must all be meaningful. Empty values,
placeholders, deferrals, stubs, failures, and contradictions do not
count as evidence.

| Acceptance ID | Status | Command or procedure | Result | Artifact |
|---|---|---|---|---|
| `<ID>-AC-01` | PASS | `<command>` | `<result>` | `<artifact>` |
