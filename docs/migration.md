# Migration Guide

Operator upgrades between the Investigation era and current Caduceus (Auto
Review + code tickets only). Soft-wrap prose at ~80 characters.

## Label standardisation

| Label | Role today |
| --- | --- |
| `autofix` (or `ticket_label_code`) | Code / handoff tickets — still active |
| `autofix-investigate` | Removed with Investigation (N+1) |
| `autoreview` | Reserved; **inert** — does not gate PR review |

Legacy emoji `🤖 auto-fix` still translates at config load to `autofix` with a
one-time warning. Re-label open issues to the canonical name; the daemon polls
only the canonical label.

## Upgrading to Release N (Investigation deprecated)

Release N deprecates Investigation but keeps it active: admissions continue with
deprecation warnings; in-flight work drains normally. Auto Review is available
and optional (`executor_mode: oci` + `sandbox:`
+ `auto_review.enabled: true`). Run `caduceus doctor` before enabling.

### Before you upgrade (pre-N → N)

1. Stop the daemon.
2. `caduceus queue show` — note investigation entries; they keep running under
   N.
3. Optional: configure OCI + `auto_review` to start reviewing PRs.

### What happens at first open (pre-N → N)

- Structural migration: SQLite v7 → v8, JSON envelope bump.
- Non-terminal Investigation rows are **not** terminated.
- Investigation admissions keep working with deprecation warnings.

## Direct upgrade pre-N → N+1 (skipping N)

Same as N+1 below: startup reconcile terminates and archives non-terminal
investigation entries on first open. See
[docs/release-notes.md](release-notes.md).

## Upgrading to Release N+1 (Investigation removed)

### Before you upgrade

1. **Stop the daemon.**
2. **Check for investigation work:**
   ```bash
   caduceus queue show
   ```
Note any entry whose ticket type is `investigation` — the upgrade
will terminate and archive those rows.
3. **Remove `ticket_label_investigation`** from config if present. That key now
   fails load (`unknown field`). Use `auto_review:` for PR review instead.

### What happens at first open

Startup reconcile runs on first store open (JSON and SQLite):

| Pre-upgrade phase (investigation rows) | Post-upgrade |
| -------------------------------------- | ------------ |
| `queued`                               | `skipped`    |
| `in_progress`                          | `skipped`    |
| `awaiting_review`                      | `skipped`    |
| `done` / `skipped` (already terminal)  | unchanged    |

Terminated rows are archived (`investigation_archived`,
`source=reconcile/startup`) and announced with a WARN-level
`review_migration_terminated_investigation` event. The pass is idempotent.

### Re-filing terminated work

Investigation is gone. For each archived investigation entry:

1. If the issue still needs automated code changes, label it `autofix` (or your
   `ticket_label_code`).
2. For PR review, enable Auto Review (OCI required) — see
   [docs/auto-review.md](auto-review.md) and `caduceus doctor`.
3. Findings from skipped investigation rows are not recovered; reopen or comment
   on the issue manually if needed.

### Rollback

N+1 stores remain loadable by release N only if no reconcile terminations have
fired. After termination, those rows appear as `skipped` under N. To re-queue
them, restore the pre-upgrade state backup before restarting under N.

### Compatibility surface

Parse-compat for old investigation rows lives in
`src/state/queue/legacy_investigation.rs`. Do not delete it while any supported
upgrade path from a pre-N or N-era store remains.
