# Migration Guide

## Upgrading to Release N (Investigation deprecated, Auto Review available)

Release N deprecates Investigation but keeps it active: admissions
continue with deprecation warnings, in-flight work drains through the
normal runtime loop. No startup termination of ordinary Investigation
work in N (DAR §4.4). Auto Review is available and optional.

### Before you upgrade (pre-N → N)

1. Stop the daemon.
2. `caduceus queue --show` — note investigation entries; they keep
   running under N.
3. Optional: set up OCI (`executor_mode: oci` + `sandbox:`) and
   `auto_review.enabled: true` to start reviewing PRs. Run
   `caduceus doctor` first.

### What happens at first open (pre-N → N)

- Structural migration: SQLite v7 → v8, JSON envelope bump.
- Non-terminal Investigation rows are NOT terminated or reconciled.
- Investigation admissions keep working with deprecation warnings.

### Label standardisation (#291)

- `autofix` (code tickets), `autofix-investigate` (investigation),
  `autoreview` (reserved-inert, Phase 1 — no dispatch effect).
- Legacy emoji labels (`🤖 auto-fix`) are translated at read time to
  the canonical name with a one-time warning. Update the config and
  re-label open issues to the canonical name.

## Direct upgrade pre-N → N+1 (skipping N)

Operators upgrading directly from pre-N to N+1 get identical handling
of pre-N rows: the startup reconcile pass (independent of the migration
chain) terminates and archives non-terminal investigation entries on
first open. See the N+1 section below and the N+1 release notes for
the direct-upgrade callout.

## Upgrading to Release N+1 (Investigation removed)

Release N+1 removes the Investigation feature. This guide covers every
supported starting point.

### Before you upgrade

1. **Stop the daemon.** No queue work runs during the upgrade.
2. **Check for investigation work.**
   ```bash
   caduceus queue --show
   ```
   Note any entry whose ticket type is `investigation`. These are the
   rows the upgrade will terminate and archive (below).
3. **Remove the config key.** If your `config.yaml` still sets
   `ticket_label_investigation`, delete the key (not the whole file).
   The load now fails deliberately when it is present — see the
   release notes for the exact error text.

### What happens at first open

The startup reconcile pass runs when the daemon first opens the store
on N+1, on both backends (JSON and SQLite):

| Pre-upgrade phase (investigation rows) | Post-upgrade |
| -------------------------------------- | ------------ |
| `queued`                               | `skipped`    |
| `in_progress`                          | `skipped`    |
| `awaiting_review`                      | `skipped`    |
| `done` / `skipped` (already terminal)  | unchanged    |

Every terminated row is archived through the standard audit seam
(`investigation_archived`, `source=reconcile/startup`) and announced
with a WARN-level `review_migration_terminated_investigation` event
carrying the repo, issue, and prior phase. Nothing is silently
dropped: the pass is idempotent and re-runs terminate 0 rows.

### Re-filing terminated work

Investigation is gone; its replacement is
[Auto Review](architecture/auto-review.md). For each archived
investigation entry:

1. Decide whether the underlying issue still needs attention. Auto
   Review requires OCI execution (`executor_mode: oci` plus a
   `sandbox:` block) — see `caduceus doctor` to check readiness.
2. If the issue should get automated code changes, re-file it as a
   code ticket by applying the `autofix` label (or your configured
   `ticket_label_code` value).
3. If the issue only needed the investigation's findings, they are
   lost — the row was skipped before execution. Re-open the issue
   manually or add a comment describing what to investigate as part
   of a code ticket's scope.

### Rollback

N+1 stores remain loadable by release N **only** if no reconcile
terminations have fired (the reconcile pass writes phases and audit
events the N-era code also understands; a rollback after termination
shows those rows as `skipped` with the removal reason in `last_error`,
which the N-era UI renders verbatim). If you must roll back and want
the rows re-queued, restore the pre-upgrade backup of the state file
before restarting under release N.

### Deleting the compatibility surface

The retained parse-compat variants are documented in
`src/state/queue/legacy_investigation.rs` with a deletion checklist
(the earliest release with no supported upgrade path from a pre-N or
N-era store). Do not delete them while any supported upgrade path
remains — doing so turns store open into a hard load failure.
