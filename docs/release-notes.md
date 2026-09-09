# Release Notes

## Release N — Auto Review available, Investigation deprecated

### Auto Review (new)

- Automated PR code review on every eligible PR revision in watched
  repositories. Canonical spec: [docs/architecture/auto-review.md](architecture/auto-review.md).
- Requires OCI execution (`executor_mode: oci` + `sandbox:` block with
  a digest-pinned image). Run `caduceus doctor` to check readiness.

### OCI-only behaviour change for default installs

Default installs run TrustedHost. Auto Review is rejected at config
load with an actionable error when `auto_review.enabled: true` is set
without OCI:

```text
auto_review.enabled requires OCI execution: set executor_mode: oci
and provide a valid sandbox: section (digest-pinned image).
TrustedHost offers no containment for reviewing untrusted PR
content. Run `caduceus doctor` or see the configuration docs
```

This is a deliberate behaviour change for default installs (DAR §6.3).

### Investigation deprecated

Investigation remains active but deprecated; admissions carry
deprecation warnings. Removed in N+1 (see below).

### Label standardisation (#291)

- Canonical labels: `autofix` (code tickets), `autofix-investigate`
  (investigation, removed in N+1), `autoreview` (reserved-inert in
  Phase 1 — no daemon code polls it and no dispatch effect).
- Legacy emoji labels (`🤖 auto-fix`) are translated at read time to
  the canonical name with a one-time warning.

## Release N+1 — Investigation removed

Investigation is no longer an active feature. The `autofix-investigate`
trigger label, the `ticket_label_investigation` config key, the
investigation worker prompt, and the investigation finalization route
are all gone. Only `autofix` (code tickets) and Auto Review (`auto_review`)
admit work.

### Config key removed

`ticket_label_investigation` was removed. A config that still carries
the key **fails to load** with this deliberate error:

```text
ticket_label_investigation was removed in release N+1; investigations
were replaced by auto_review — remove the key
(docs/architecture/auto-review.md §12)
```

Remove the key from your config file. There is no replacement value to
set; investigation work is replaced by
[Auto Review](architecture/auto-review.md), which is enabled with an
`auto_review:` block (`enabled: true`) and requires OCI execution.

### `autofix-investigate` label removed

The daemon no longer polls the `autofix-investigate` label and rejects
any attempt to admit an investigation ticket at the store level
(`investigation_admission_rejected` audit event). Delete the label from
your repositories, or leave it — it is inert.

### Startup reconcile pass (direct-upgrade callout)

**Operators upgrading directly from pre-N to N+1** (skipping release N):
store open terminates and archives any non-terminal investigation
entry (`review_migration_terminated_investigation`); nothing is
silently dropped.

Concretely, on the first open after upgrade every non-terminal
investigation queue entry (queued, in-progress, or awaiting-review) is:

1. moved to the `skipped` phase, with `last_error` naming this removal
   (`investigation removed in N+1 (#331); terminated by the startup
   reconcile pass (DAR §4.4) — no work was executed`),
2. archived through the standard audit seam
   (`investigation_archived`, `source=reconcile/startup`), and
3. reported with the operator-visible termination event
   (`review_migration_terminated_investigation`, WARN level, carrying
   the repo, issue, and the phase the row had before termination).

The pass is idempotent: a re-open terminates 0 rows. It is independent
of the schema migration chain, so it behaves identically for pre-N →
N+1, N-era → N+1, and stores that were migrated while release N was
running. See [migration.md](migration.md) for the full upgrade
procedure and how to re-file terminated work.

### Retained for compatibility (not an oversight)

The following survive **only** so older persisted state keeps loading.
They are terminal-never-admitted; no production path creates new
values:

- `TicketType::Investigation` (queue rows),
- the `FinalizationStage::InvestigationReady` /
  `InvestigationCommented` variants (checkpoints),
- the `investigation` field in `worker-result.json` (ignored on read),
- the `proposed_investigation_comment` field in dry-run preview
  reports (always `null`).

Each carries a pointer to the removal checklist in
`src/state/queue/legacy_investigation.rs`. They can be deleted in the
earliest release with no supported upgrade path from a pre-N or N-era
store.
