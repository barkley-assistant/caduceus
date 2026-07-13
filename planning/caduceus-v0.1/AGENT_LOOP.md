# One-Task Agent Prompt

You are implementing Caduceus v0.1 one verified work item at a time.

**You do the work yourself.** Read files, edit files, run `cargo`, run `pytest`, commit, push. Do not delegate, route, or hand off to any other coding agent or CLI tool — there is none. Words like "implement", "code", "edit", or "write tests" refer to actions you perform with your own tools, not calls to `claude-code`, `opencode`, `codex`, or any external service.

Start at the repository root. The controller is the only work selector:

```text
python3 -B planning/caduceus-v0.1/tools/validate_plan.py
python3 -B planning/caduceus-v0.1/tools/next_task.py --format json
```

- `kind: task`: claim exactly its `id`, then read every returned path.
- `kind: phase_gate`: claim `phase-XX`, run only the referenced phase gate, and write the returned handoff.
- `kind: blocked`: do not select later work; report the listed blocker and stop.
- `kind: done`: report completion and stop.
- `resumed: true`: continue the existing item after inspecting the workspace and any existing handoff notes; do not start over destructively.

Claim and finish through the guarded controller:

```text
python3 -B planning/caduceus-v0.1/tools/set_status.py <id-or-phase-XX> in_progress
python3 -B planning/caduceus-v0.1/tools/set_status.py <id-or-phase-XX> complete --handoff handoffs/<file>.md
```

1. Run the plan validator and next-task selector.
2. Claim the returned work item through `tools/set_status.py`; never edit progress.json manually.
3. Work only on the returned task or phase gate.
4. For a task, read CONTRACTS.md, the referenced phase file, the task packet, and dependency handoffs before editing.
5. Treat CONTRACTS.md and task-manifest.json as authoritative. Archived planning material is context only.
6. Do not change public schemas, lifecycle transitions, CLI/environment contracts, dependency edges, or file ownership silently.
7. Use RED → GREEN → REFACTOR where tests are possible. A RED failure must demonstrate missing behavior, not unrelated compilation failure.
8. Run every named acceptance check. Never mark work complete because code merely looks correct.
9. Preserve unrelated workspace changes and do not perform destructive Git operations.
10. Write the required handoff with files, signatures, schema/state effects, commands, results, and residual risks, then transition status through `tools/set_status.py`.
11. Stop after this one work item. Do not select or begin the next item in the same context.

If acceptance is not yet passing but further implementation is possible, leave the item `in_progress` so the next fresh invocation resumes it. If genuinely blocked, make all safe read-only checks first, write the handoff, and transition the item to `blocked`. Resume a resolved blocker with `set_status.py <id> pending`. Never invent a missing contract or mark incomplete work complete.

## Contract mismatch and revision

A `CONTRACTS.md` hash mismatch is a safety stop. Do not edit `contracts_sha256`, `CONTRACT_REVISIONS.md`, or the archive to make the validator pass. Record the concrete contradiction and its affected task IDs in a bounded blocker handoff. Only an explicitly authorized reviewer may approve a revision, record it in `CONTRACT_REVISIONS.md`, update every affected contract/task/phase/manifest/documentation surface, refresh `contracts_sha256`, and re-run the validator. The archive digest is immutable.

## Human-review checkpoints

When a manifest task declares `human_review.required`, complete the implementation checks and ordinary handoff, then leave the task `in_progress`. A human reviewer—not the autonomous implementation loop—must write the declared review artifact before `set_status.py ... complete` will succeed. Do not treat an agent-authored review artifact as satisfying this checkpoint.
