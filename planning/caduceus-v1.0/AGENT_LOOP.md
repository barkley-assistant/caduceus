# Optional one-task agent loop

This local helper is optional. Public contributors may instead use each task
packet directly as a GitHub issue or SDD work unit. The manifest and progress
ledger help sequence local agent work; they do not replace task requirements.

## Begin

From the repository root, run:

```text
python3 -B planning/caduceus-v1.0/tools/validate_plan.py
python3 -B planning/caduceus-v1.0/tools/next_task.py --format json
```

Stop on `kind: blocked` or `kind: done`. Otherwise handle exactly the
returned task or phase gate. The controller, not the implementation
agent, selects work.

## Task procedure

1. Claim the returned task with `set_status.py <id> in_progress`.
2. Read `CONTRACTS.md`, the phase file, task packet, and dependency
   handoffs in full.
3. Inspect the workspace and preserve unrelated changes.
4. Implement only the packet scope. Cross-owned edits require an
   explicit contract reason in the handoff.
5. Run every named acceptance check and the repository gate appropriate
   to the changed files.
6. Write `handoffs/<id>.md` from `handoffs/TEMPLATE.md`. Record one
   `PASS` evidence row for every manifest acceptance ID.
7. If independent review is required, leave the task `in_progress`
   until a reviewer completes the declared human-review artifact.
8. Complete the task with `set_status.py <id> complete --handoff
   handoffs/<id>.md`.
9. Start a fresh context for the next work item.

Use `blocked` only for a genuine contract or external blocker and
record bounded evidence in the handoff. Do not edit `progress.json`
directly.

## Phase gates

After all phase tasks complete, the selector returns the phase gate.
Claim it, run only the checks in the phase specification, record one
`PASS` row for every phase acceptance ID, and complete it through
`set_status.py`. Do not start the next phase in the same invocation.

## Hard stops

Stop and report a contract digest mismatch, draft catalog, controller
blocker, failed required check, missing independent review, or
contradiction between a task and `CONTRACTS.md`. Never modify the v0.1
planning archive or refresh a digest without an authorized contract
revision.
