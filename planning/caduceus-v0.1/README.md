# Caduceus v0.1 Execution Plan

This directory is the authoritative implementation plan for a one-task-at-a-time agent loop.

## Authority

1. [CONTRACTS.md](./CONTRACTS.md) owns all cross-task invariants, schemas, public interfaces, state transitions, failure classifications, and Hermes lifecycle behavior.
2. [task-manifest.json](./task-manifest.json) owns task IDs, dependencies, execution phases, file ownership, and primary tests.
3. The current file under [tasks/](./tasks/) owns task-local behavior and acceptance criteria.
4. [phases/](./phases/) owns phase entry/exit gates.
5. [progress.json](./progress.json) records execution state only; it never changes requirements.
6. [CONTRACT_REVISIONS.md](./CONTRACT_REVISIONS.md) records approved revisions to the sealed contract and required re-verification.
7. [archive/full-reviewed-plan.md](./archive/full-reviewed-plan.md) preserves review history and the original detailed playbook, but is not an implementation authority.

If a task conflicts with CONTRACTS.md, stop and report the conflict. Do not silently redesign a contract.

## Contract mismatch

A `contracts_sha256` mismatch is a safety stop. An implementation agent must not repair it by editing the digest. Record the contradiction as a bounded blocker and await an explicitly authorized contract revision. The reviewer follows the revision-control procedure in [CONTRACTS.md](./CONTRACTS.md), records it in [CONTRACT_REVISIONS.md](./CONTRACT_REVISIONS.md), updates all affected plan surfaces, then refreshes the contract digest and runs the validator. The archived reviewed monolith is immutable and its digest is never refreshed.

## One-task loop

Each agent invocation handles exactly one returned work item:

1. Run `python3 planning/caduceus-v0.1/tools/validate_plan.py`.
2. Run `python3 planning/caduceus-v0.1/tools/next_task.py --format json`.
3. Claim the returned item with `python3 planning/caduceus-v0.1/tools/set_status.py <id> in_progress` (phase gates use `phase-XX`).
4. If the result is a task, load CONTRACTS.md, its phase file, its task packet, and every dependency handoff.
5. Inspect the workspace before editing. Preserve unrelated user changes.
6. Implement only the task scope and owned files. Cross-owned edits require an explicit contract reason in the handoff.
7. Run the task checks plus repository-wide formatting/lint/test checks appropriate to the files now present.
8. Write `handoffs/<task-id>.md` using [handoffs/TEMPLATE.md](./handoffs/TEMPLATE.md).
9. Record success with `set_status.py <id> complete --handoff handoffs/<id>.md`. Use `blocked` only for a genuine contract/external blocker, with evidence in the handoff. Do not edit progress.json directly.
10. Start a fresh agent context for the next work item. Do not implement two tasks in one invocation.
11. When next_task returns a phase gate, run only that phase's gate, write `handoffs/phase-XX.md`, and complete it through `set_status.py` before continuing.

## Progress states

- `pending`: untouched or ready.
- `in_progress`: the current invocation owns it.
- `complete`: acceptance and verification passed and a handoff exists.
- `blocked`: cannot proceed without a contract decision or external change; the handoff explains why.

Only one task or phase gate may be `in_progress`.

To resume after a blocker is resolved, run `set_status.py <id> pending`; the selector will return it again when its dependencies remain satisfied.

## Agent prompt

Use [AGENT_LOOP.md](./AGENT_LOOP.md) as the stable task prompt for any compatible coding agent. Give each invocation a fresh context and the repository root as its workspace.
