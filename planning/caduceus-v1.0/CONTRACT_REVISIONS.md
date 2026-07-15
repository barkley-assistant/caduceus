# Contract revision history

## Initial v1.0 correctness-and-scale specification

- **Date:** 2026-07-15
- **Authority:** project owner approval in the public planning session
- **Rationale:** define v1.0 around verified runtime correctness, durable state,
  bounded single-host scale, executor isolation, and reproducible evidence.
- **Requirements:** `PLAN`, `CI`, `RUN`, `STATE`, `FINAL`, `SCHED`, `REPO`,
  `GH`, `EXEC`, `CONFIG`, and `ACCEPT` requirement families.
- **Planning surfaces:** the contract, eight phase documents, 42 standalone task
  packets, manifest, progress ledger, templates, and validation tools.
- **Carryover debt:** `DEBT-MSRV` belongs only to Task 0.3, `DEBT-STATUS` only
  to Task 2.7, `DEBT-ATOMIC` only to Task 3.1, and `DEBT-RETENTION` only to
  Task 3.6.
- **Historical integrity:** `planning/caduceus-v0.1/` remains an immutable,
  digest-sealed implementation archive.
- **Verification:** validate the contract digest, complete requirement mapping,
  task and phase acceptance IDs, local links, dependency graph, progress parity,
  and sealed v0.1 tree before implementation begins.

## Public review hardening

- **Date:** 2026-07-15
- **Authority:** project owner correction request after final public review
- **Rationale:** make commit validation, migration ambiguity, credential
  transport, private storage, OCI isolation, orphan recovery, finalization
  correlation, and degraded-dependency behavior decision-complete.
- **Requirements:** adds `CI-003` and clarifies `RUN-004`, `STATE-002`,
  `FINAL-001`, `SCHED-002`, `REPO-001`, `GH-001`, and `EXEC-002`.
- **Planning surfaces:** updates the affected tasks, Phase 01, Phase 06,
  controller durability, manifest acceptance IDs, and requirement mapping.
- **Review gates:** remains limited to Tasks 2.4, 3.4, 6.4, and 7.5.
- **Verification:** rerun plan validation, selector, negative controller tests,
  Python compilation, links, Markdown width, task/debt/review counts, and the
  sealed v0.1 tree check before accepting the refreshed contract digest.

## Hermes installation incident

- **Date:** 2026-07-15
- **Authority:** project owner request after a real Hermes installation failure
- **Rationale:** move production configuration, transactional cron registration,
  host-capability diagnosis, and installed-path proof ahead of worker runtime
  corrections and release claims.
- **Requirements:** adds `INSTALL-001`, `HERMES-001`, `HERMES-002`, and
  `ACCEPT-003` without relying on the v0.1 archive.
- **Planning surfaces:** adds Tasks 1.4, 2.1, and 2.2; renumbers the prior Phase
  02 tasks to 2.3–2.8; expands Phase 01, Phase 02, Phase 07, release tests,
  dependencies, review ownership, debt ownership, and requirement mappings.
- **Review gates:** remains exactly Tasks 2.4, 3.4, 6.4, and 7.5.
- **Verification:** prove the active 42-task catalog, exact progress parity,
  acyclic dependencies, pinned-host negative scenarios, links, Python tools,
  Markdown width, and sealed v0.1 tree before refreshing the digest.

## Hermes transaction and release traceability hardening

- **Date:** 2026-07-15
- **Authority:** project owner correction request after Hermes-scope review
- **Rationale:** make configuration failure precedence, cron ambiguity,
  wrapper/job rollback, doctor sentinels, disposable gateway ownership, and
  canary side effects decision-complete.
- **Requirements:** clarifies `INSTALL-001`, `HERMES-001`, `HERMES-002`, and
  `ACCEPT-003`; no new implementation scope or review gate is introduced.
- **Planning surfaces:** renames Task 1.4, strengthens Tasks 2.1, 2.2, 7.2,
  7.3, 7.5, and 7.6, expands Phase 07 evidence, exact requirement mappings,
  and plan-validator packet/order integrity checks.
- **Review gates:** remains exactly Tasks 2.4, 3.4, 6.4, and 7.5.
- **Verification:** run exact-set/order negative tests plus the active catalog,
  selector, parity, review/debt, link, Python, Markdown, cleanup, and v0.1 seal
  checks before refreshing the digest.

## Final readiness boundary

- **Date:** 2026-07-15
- **Authority:** project owner final bounded readiness refinement
- **Rationale:** make Phase 00 capability truth and Phase 01 executable
  reachability explicit before implementation begins, without adding tasks or
  pretending known implementation gaps are fixed.
- **Requirements:** adds `PLAN-005`, extends the pinned Hermes fixture, and
  completes `ACCEPT-002` review and canary evidence mappings.
- **Planning surfaces:** refines Task 0.1, Task 1.4, Phase 00, Phase 01,
  readiness attachments, requirement mapping, and review validation.
- **Review gates:** remains exactly Tasks 2.4, 3.4, 6.4, and 7.5.
- **Verification:** prove artifact/gap ownership, walking-skeleton reporting,
  completed-review enforcement, exact packet integrity, active catalog, links,
  Python, Markdown, cleanup, and v0.1 seal before refreshing the digest.

## Production-surface quality

- **Date:** 2026-07-15
- **Authority:** project owner addition within the final readiness refinement
- **Rationale:** ensure shipped plugin hooks, scripts, assets, skills, generated
  text, comments, errors, manifest fields, and command paths are
  production-ready rather than carrying development or planning artifacts.
- **Requirements:** adds `QUALITY-001` and assigns its audit, correction,
  scanner, lifecycle, documentation, and release checks to existing tasks.
- **Task count and review gates:** remains 42 tasks with reviews exactly at
  Tasks 2.4, 3.4, 6.4, and 7.5.
- **Verification:** run positive/negative scanner planning checks, installed
  lifecycle requirements, exact mappings, and the complete readiness gate.

## v0.1 ↔ v1.0 migration-command split

- **Date:** 2026-07-15
- **Authority:** project owner correction request before staging the v1.0 rework
- **Rationale:** the v1.0 contract (`STATE-002`) names the v1.0 subcommand as
  `caduceus migrate-state --to sqlite`, but the shipped binary exposes
  `caduceus migrate-state --from <legacy.json> [--dry-run]`. Operator docs
  (`MIGRATION.md`, `docs/state-recovery.md`) reference the shipped flag set
  while the contract references the v1.0 flag set. Without an explicit
  cross-reference, an operator reading the contract would search for a
  flag the binary does not implement, and an operator reading the docs
  would conclude `--from` is the v1.0 form.
- **Requirements:** clarifies `STATE-002`; adds no new requirement. The
  shipped-binary flag set (`--from <legacy.json>`) remains the canonical
  v0.1 surface; the v1.0 flag (`--to sqlite`) becomes available only when
  Task 3.3 ships.
- **Planning surfaces:** adds a v0.1↔v1.0 cross-reference note inside the
  `STATE-002` requirement, adds a "Canonical command" callout to the Task
  3.3 packet, and updates `MIGRATION.md` and `docs/state-recovery.md` so
  each states the v0.1 flag set as the *currently supported* command and
  flags `--to sqlite` as a v1.0 planned interface.
- **Review gates:** unchanged. Remains exactly Tasks 2.4, 3.4, 6.4, and 7.5.
- **Verification:** rerun `validate_plan.py` against the refreshed
  contract digest; confirm 42 tasks, 8 phases, acyclic, phase-safe;
  confirm `MIGRATION.md` and `docs/state-recovery.md` mention only the
  shipped flag set when describing current capability.

## GH-001 endpoint allowlist

- **Date:** 2026-07-15
- **Authority:** project owner request to make `api_base` host restriction
  explicit before Task 2.3 / worker work begins
- **Rationale:** v0.1 describes `api_base` as "primarily useful for GitHub
  Enterprise Server installs" with no validation requirement. v1.0 needs
  a positive allowlist (GitHub.com + GHES) so the daemon refuses
  non-GitHub REST surfaces, custom GitHub-shaped shims, and path-prefixed
  proxies at configuration load time rather than failing later in
  mysterious ways. Forbidden-string matching against
  `comment_forbidden_strings` is not acceptable as a substitute; that
  mechanism is for outbound public text, not endpoint validation.
- **Requirements:** extends `GH-001` with a "Supported GitHub API
  endpoints" subsection that names GitHub.com and GHES as the two
  allowed endpoint families and forbids everything else.
- **Planning surfaces:** adds acceptance check `5.5-AC-05` to Task 5.5
  (positive allowlist validator; explicit "no forbidden-string matching"
  prohibition); updates `docs/configuration.md` §"`api_base`" to spell
  out the allowlist rule; no new task, no new requirement, no new review
  gate.
- **Review gates:** unchanged. Remains exactly Tasks 2.4, 3.4, 6.4, and 7.5.
- **Verification:** rerun `validate_plan.py` against the refreshed
  contract digest; confirm Task 5.5 now has five acceptance IDs and that
  the manifest's `requirement_map` for `GH-001` includes `5.5-AC-05`.
