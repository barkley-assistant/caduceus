# Commit-Policy Exceptions

This document records accepted deviations from the commit-policy gate
(`scripts/check-commits.sh`, required check `commit-policy / check`,
AGENTS.md "Commits"). An exception is granted only when a non-conforming
subject is already merged and a history rewrite is not justified. It is
a record of past decisions, not an allowlist: new commits and merge
subjects must conform to `<type>(<scope>): <description>`.

| Commit | Subject | Merged | Reason |
| --- | --- | --- | --- |
| 48ccea37d02baebdc89f5c257954f459245893b5 | `test: live adversarial Docker certification suite + CI gates (closes #252)` | 2026-09-02 | Squash-merge subject lacks a scope. Merged before branch protection required `commit-policy / check` on main (protection enabled 2026-09-03). The change passed every gate (live OCI certification 30/30, full CI matrix) and closed issue #252. |

## Decision record (2026-09-03)

Remediation chosen: **documented exception** (this file).

History rewrite was rejected:

- The State-Recovery wiki process governs daemon state (`state.json` /
  `state.db` corruption, queue reset, lock cleanup); it documents no
  procedure for rewriting git history on `main`.
- Rewriting the subject of `48ccea3` requires a force-push to `main`,
  which would invalidate CI statuses and daemon checkpoints anchored to
  the commit SHA, contradict the branch protection now in force
  (`allow_force_pushes: false`), and leave no supported recovery path if
  the rewrite goes wrong.
- The deviation is bounded: a single squash subject merged before
  enforcement, for a change that itself satisfied all gates.