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

The validator enforces all of the rules below. A handoff that fails
any of them is rejected at `set_status.py` and the transition is
not recorded. Empty values, placeholders, deferrals, stubs,
failures, and contested claims do not count as evidence.

Rules in force (single source of truth:
`planning/caduceus-v1.0/tools/validate_plan.py`):

1. One five-column evidence row per `acceptance_ids` entry. Rows
   that match no acceptance ID are ignored; rows that match
   multiple are rejected.
2. The `Status` column MUST be exactly `PASS` or `PASSED`. Every
   other value (including `passing`, `n/a`, `deferred`,
   `stubbed`, `failed`, or any of `deferred`, `stubbed`,
   `failed`, or `contradicted` anywhere in the procedure, result,
   or artifact cell) is rejected.
3. The `Command or procedure` cell must name a real command,
   function path, or procedure the reader can run. Backticks and
   pipe characters inside cells are discouraged because the
   validator's cell splitter treats `|` as a column separator.
4. The `Result` cell must record an observed outcome (what the
   command printed, what the test reported, what the validator
   said) — not a restated plan.
5. The `Artifact` cell must point at a durable file the reader
   can inspect: a handoff path, a test file, an attachment, or a
   commit SHA. Generic values such as "pass", "passed",
   "success", or the literal acceptance ID are rejected.
6. For tasks whose `human_review.required` is true, the
   `HUMAN_REVIEW_TEMPLATE.md` review artifact must exist and
   its `Implementation handoff:` field must equal this file's
   path. The reviewer's actor and reviewer identities must
   differ; the commit must be a real 40- or 64-hex SHA (not
   all-zero); the decision must be `approved` or `approved with
   notes`; the approval provenance must be a `https://...` URL
   or `PR #N`.

Forbidden token list (the validator scans these literally in
the procedure, result, and artifact cells):

- `deferred`, `stubbed`, `failed`, `contradicted` (case-folded
  word match; any cell containing one of these is rejected).
- The placeholders: empty string, `-`, `n/a`, `na`, `none`, `not
  applicable`, `tbd`, `todo`, `placeholder`, and any value
  shorter than 3 alphabetic characters.

Acceptable evidence shape (copy verbatim, then fill in):

| Acceptance ID | Status | Command or procedure | Result | Artifact |
|---|---|---|---|---|
| <ID>-AC-01 | PASS | <command> | <observed result> | <handoff path or test ref> |
