# Contract revisions

This is the approval record for changes to the sealed cross-task contract. It is not a task handoff. Each entry must identify the approving reviewer, rationale, affected work, and re-verification required before the contract digest is updated.

## CR-001 — 2026-07-13 — approved plan-control clarification

- Approver: project reviewer
- Rationale: make contract-drift handling, inbound comment filtering, outbound public-voice matching, legacy Hermes migration, worker-result retry semantics, and the process-supervisor review checkpoint unambiguous before implementation.
- Affected task packets: 0.2, 1.1, 5.1, 5.3, 5.6, 6.6, 7.1.
- Affected phase gate: 05-workers.
- Required re-verification: plan validator; controller rejection of Task 5.1 completion without its human-review artifact; structural and matching tests named in the affected packets.
- Archive: unchanged; `archive/full-reviewed-plan.md` remains immutable.
