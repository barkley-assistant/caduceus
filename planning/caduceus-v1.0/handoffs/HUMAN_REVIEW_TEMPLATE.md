# Human-review template

This template is for tasks that declare
`human_review.required`. The human reviewer — not the
implementation agent — fills this in. The agent's
implementation handoff lives at `handoffs/<task-id>.md`.

## Reviewer

- Implementation actor:
- Reviewer name / handle:
- Review date:
- Reviewed commit:
- External approval provenance: `https://...` or `PR #123`
- Implementation handoff: `handoffs/<task-id>.md`

## Acceptance checklist

- [ ] The task packet's "Outcome and required behavior"
  section was read in full.
- [ ] Every named acceptance check in the task packet
  was run and recorded in the implementation handoff.
- [ ] The implementation handoff lists every file
  changed, every public signature affected, the
  test matrix, the exact commands run, and the
  residual risks.
- [ ] Every required check passed; none is deferred, stubbed, failed,
  contradicted, or represented by a placeholder.
- [ ] State, CLI, configuration, worker, and executor behavior touched
  by the task is consistent with the current v1.0 requirement IDs.
- [ ] The reviewer is independent from the implementation actor and
  reviewed the exact commit and handoff named above.

## Decision

- Decision: approved | approved with notes | rejected

Only an independent reviewer fills this field. The validator accepts
`approved` or `approved with notes` for task completion.

## Notes
