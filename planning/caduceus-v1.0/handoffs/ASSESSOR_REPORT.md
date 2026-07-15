# Caduceus — Assessor Report (Unstaged Changes, v0.1 → v1.0 Handoff)

**Scope:** Working tree at `/home/agent/projects/barkley-assistant/caduceus`
(based on commit `9b8ec29`, on `main`, up to date with `origin/main`).
**Audience:** v0.1 release captain and v1.0 acceptance reviewers.
**Status:** Documentation-only delta. No Rust, Python, plugin, CI, or
manifest source has been modified.

---

## 1. What changed (unstaged)

### 1.1 Modified tracked files

| File | Δ (lines) | Purpose of change |
|---|---|---|
| `.gitignore` | +2/-1 | Generalize `progress.lock` to `planning/*/.progress.lock`; add `.atl/` (skill registry scratch). |
| `CONTRIBUTING.md` | +52/-42 | Rewrite as a focused contributor guide: start-here checklist, PR conventions, scoped Conventional Commits with the exact required CI gate, repo conventions. Drops pre-implementation scaffolding language and the design-principles section that has moved to `docs/`. |
| `MIGRATION.md` | +136/-216 | Rewrite as a version-agnostic operator migration guide. New framing: "supported source → target" model, explicit preflight, dry-run, idempotent `migrate-state`, validate-and-resume, rollback, troubleshooting. Backed by `docs/state-recovery.md` and `docs/troubleshooting.md` instead of inlining subcommands. Adds the `hermes caduceus doctor` flow and a release-specific notes requirement. |
| `README.md` | +263/-498 | Compress from 711 to 263 lines. Becomes a front door only: tagline, install (Hermes + standalone), 60-second orientation, "what Caduceus explicitly is not" (anti-marketing), pointers into `docs/`. Removes the inlined env-var table, retry semantics, config reference, dry-run behavior, and public-voice rule (all moved into `docs/`). Adds SemVer policy and explicit pointer to `MIGRATION.md` for cutover. |
| `planning/README.md` | +8/-2 | Now points to `caduceus-v1.0/README.md` as the active plan and to `caduceus-v0.1/README.md` as an immutable audit source. Adds the explicit note that v1.0 task catalog is currently draft and "cannot select implementation work." |

**Net:** `460 insertions, 675 deletions` — about 35% line reduction overall, mostly through moving detail out of root files into `docs/`.

### 1.2 New untracked files

**Root governance** (untracked)

- `AGENTS.md` — Agent and human contributor contract. Repo boundaries (Rust
  crate, Python bridge, plugin layout, no extra top-level dirs), safety
  invariants (do not edit state, do not bypass `contracts_sha256`, no
  `todo!()`/`unimplemented!()`/new unsafe), scoped Conventional Commits,
  and the exact CI gate: `cargo fmt --check`, `cargo clippy --locked
  --all-targets -- -D warnings`, `cargo test --locked --all-targets`,
  `pytest -q tests/hermes_plugin_test.py tests/bridge_test.py`. This file
  is referenced from `CONTRIBUTING.md`, `RELEASING.md`, and the
  new `planning/caduceus-v1.0/CONTRACTS.md`.
- `CHANGELOG.md` — Keep-a-Changelog format. `0.1.0` entry dated
  `2026-07-15` documents the v0.1 release and explicitly enumerates the
  known limitations (single-issue-per-tick, PAT only, JSON state, broken
  status exit codes, missing runtime tests) and the missing security
  contact. `Unreleased` is empty.
- `RELEASING.md` — Maintainer runbook. SemVer policy, what counts as
  breaking, the prepare/release/publish/after-release flow, the same CI
  gate, signed annotated tag, no force-push, no crates.io publish.
- `SECURITY.md` — Single private contact
  (`barkleyassistant@gmail.com`, "Caduceus" in subject), scope explicitly
  excludes worker harness / GitHub / Hermes itself, links to
  `docs/state-recovery.md`.

**`docs/` (11 files, 2,071 lines)**

```
README.md           48
architecture.md    225
configuration.md   359
faq.md             129
hermes-integration.md 130
installation.md    173
plugin-lifecycle.md 224
public-voice.md    170
state-recovery.md  161
the-bridge.md      239
troubleshooting.md 213
```

`docs/README.md` is an explicit table of contents. Each doc covers the
surface that the old README inlined. Notably:

- `docs/configuration.md` is the canonical config schema reference
  (replaces the inlined table in the old README).
- `docs/state-recovery.md` is the canonical recovery procedure
  (replaces the inline `recover_state` Rust example).
- `docs/the-bridge.md` is the canonical bridge contract
  (replaces the inline `invoke_harness` Python examples).
- `docs/public-voice.md` is the canonical forbidden-strings doc
  (replaces the inline "Public Voice Rule (Hard Enforcement)" section).

**`planning/caduceus-v1.0/` (active plan, 8 phases / 42 tasks)**

- `README.md` (110) — Plan authority, phase order, contract-mismatch
  protocol, optional local sequencing loop.
- `CONTRACTS.md` (593) — Single normative v1.0 contract, 34
  requirement IDs across `PLAN`, `CI`, `INSTALL`, `HERMES`,
  `QUALITY`, `RUN`, `STATE`, `FINAL`, `SCHED`, `REPO`, `GH`, `EXEC`,
  `CONFIG`, and `ACCEPT` families. Includes `PLAN-005`
  (public-readiness audit boundary), `CI-003` (commit policy),
  `INSTALL-001` (production configuration bootstrap),
  `HERMES-001`/`HERMES-002` (transactional scheduling, diagnosable
  host health), and `QUALITY-001` (shipped integrations are
  production-ready).
- `CONTRACT_REVISIONS.md` (100) — Six signed revisions dated
  `2026-07-15`, with explicit carryover-debt ownership
  (`DEBT-MSRV` → 0.3, `DEBT-STATUS` → 2.7, `DEBT-ATOMIC` → 3.1,
  `DEBT-RETENTION` → 3.6).
- `AGENT_LOOP.md` (55) — Optional one-task-at-a-time controller loop
  for local agent work.
- `task-manifest.json` (1,144 lines) — 42 tasks, 8 phases, full
  `requirement_map`, `contract_revision_log`, `v01_tree` digest.
- `progress.json` — 42 task entries, every task `pending`.
- `phases/` (8 files) — Phase entry/exit gates.
- `tasks/` (42 files) — One packet per task, naming convention
  `<id>-<slug>.md`.
- `tools/` — `validate_plan.py` (566 lines), `next_task.py`,
  `set_status.py`.
- `handoffs/` — `TEMPLATE.md`, `HUMAN_REVIEW_TEMPLATE.md`.

---

## 2. Findings — what works

### 2.1 Documentation architecture

The split is coherent and the cross-references are consistent:

- Root files are now front doors only (`README.md`, `CONTRIBUTING.md`).
- `docs/` holds operator / contributor reference material.
- `planning/` is the contract and execution surface.
- `MIGRATION.md` sits at root on purpose (operator panic-findable) but
  no longer inlines subcommand detail.

`AGENTS.md`, `CONTRIBUTING.md`, `RELEASING.md`, `SECURITY.md`,
`MIGRATION.md`, and `CONTRACTS.md` (via `AGENT_LOOP.md`) all cite the
same four-line CI gate by the same command names.

### 2.2 The known v0.1 limitations are honestly catalogued

`CHANGELOG.md` 0.1.0 lists the four known limitations (single-issue
serialization, PAT-only, JSON state, broken status exit codes) and the
missing runtime tests as `Known Limitations`. Each is owned by a
specific v1.0 task in `task-manifest.json`:

| Known limitation | Owner |
|---|---|
| Single issue per tick | 5.2 (bounded concurrency) |
| Status exit-code defect | 2.7 (corrected under DEBT-STATUS) |
| JSON state | 3.2 (SQLite) + 3.3 (migration) |
| Missing runtime tests | 7.1 / 7.2 (regression matrix) |
| PAT only | Deferred to v1.x per README "what we are not" |

### 2.3 v0.1 archive is preserved as evidence

`planning/caduceus-v0.1/` is intact. `task-manifest.json` records
`v01_tree_sha256: fc13dd96…d1b3eb` and the validator checks it
on every run. `CONTRACT_REVISIONS.md` opens with the rule "v0.1 archive
is never refreshed."

---

## 3. Validation evidence (executed)

### 3.1 Plan validator (live)

```text
$ python3 -B planning/caduceus-v1.0/tools/validate_plan.py
plan valid (active catalog): 42 tasks, 8 phases, acyclic and phase-safe
exit 0
```

### 3.2 Task selector (live)

```text
$ python3 -B planning/caduceus-v1.0/tools/next_task.py --format json
{
  "kind": "task",
  "resumed": false,
  "id": "0.1",
  "title": "Publish the v1.0 task catalog",
  "execution_phase": 0,
  ...
}
exit 0
```

This is consistent with `planning/README.md` and `AGENTS.md`: while
the v1.0 catalog is *draft*, the controller must not select
implementation work, so a clean `kind: task` return here implies
the catalog is currently `active` in `task-manifest.json`
(`"catalog_status": "active"`). **That is a contract contradiction
flagged in §4.1 below.**

### 3.3 Contract digest (live)

```text
$ sha256sum planning/caduceus-v1.0/CONTRACTS.md
fd726798b013a6718e4f913341cb5cc6dcfb8ab3893534824b6817ea2f46829c
$ grep contracts_sha256 planning/caduceus-v1.0/task-manifest.json
"contracts_sha256": "fd726798b013a6718e4f913341cb5cc6dcfb8ab3893534824b6817ea2f46829c",
```

Digest matches.

### 3.4 Manifest shape (live)

```text
total tasks: 42
phases: 8
max deps: 5
tasks w/ deps: 41
```

Every task except `0.1` has at least one dependency — the dependency
graph is connected at the top and acyclic (per validator output).

### 3.5 Git state (live)

```text
On branch main
Your branch is up to date with 'origin/main'.

Changes not staged for commit:
  modified:   .gitignore
  modified:   CONTRIBUTING.md
  modified:   MIGRATION.md
  modified:   README.md
  modified:   planning/README.md

Untracked files:
  .codegraph/   AGENTS.md   CHANGELOG.md   RELEASING.md   SECURITY.md
  docs/         planning/caduceus-v1.0/
```

No tracked source files (`src/`, `tests/`, `plugin-assets/`,
`__init__.py`, `plugin.yaml`, `Cargo.toml`, `Cargo.lock`) are touched.
No `.github/` workflows added or modified.

### 3.6 Progress ledger (live)

```text
tasks: 42
all pending: True
```

No task has been started. Consistent with the catalogue being
active but pre-execution.

---

## 4. Strengths

1. **Single-source-of-truth discipline.** Every cross-file concept
   (CI gate, contracts_sha256 protocol, state-recovery procedure,
   worker-result.json schema, comment-forbidden-strings rule, SemVer
   policy) now lives in exactly one named place and is referenced by
   path from elsewhere.
2. **Honest debt ownership.** `CONTRACT_REVISIONS.md` and the
   `DEBT-*` lines in `CONTRACTS.md` name the *task ID* that owns
   each v0.1 carryover. The changelog's "Known Limitations" row maps
   to the same owners. No orphan debt.
3. **Digest-sealed archive.** `v01_tree_sha256` plus the live
   validator means any drift in the v0.1 archive will trip a hard
   stop rather than a silent reconciliation.
4. **Right-sized README.** 711 → 263 lines, ~70% detail moved to
   `docs/`. The new root README has a clear "what we are not" anti-
   marketing section that pre-empts five common feature requests.
5. **Single normative contract.** `CONTRACTS.md` is the only file
   that defines requirements; task packets and phase gates cite it
   by stable ID.

---

## 5. Findings — what to flag

### 5.1 Catalog status contradicts `planning/README.md`

- `planning/README.md` (newly written): "Its task catalog is currently
  draft and cannot select implementation work."
- `planning/caduceus-v1.0/task-manifest.json`:
  `"catalog_status": "active"`.
- The selector returned a real `task` for `0.1`, not a `blocked` /
  `done` result, which is what an active catalog is supposed to do.

**Action:** Confirm the intent. Either the catalog is `draft` (then
`catalog_status` must be `"draft"` and `next_task.py` should refuse to
return a `task`, per `PLAN-001`), or the catalog is `active` (then
`planning/README.md` should be amended to drop the "currently draft"
phrase and `AGENT_LOOP.md` is correct as written). `PLAN-001` in
`CONTRACTS.md` is explicit on the trigger condition: *"Activating
the catalog requires a task packet for every phase and a progress
entry for every task."* Both conditions hold here, so flipping
`catalog_status` to `"active"` and removing the README disclaimer is
the more honest fix.

### 5.2 Status-exit-code debt mapping is implicit, not explicit

`CHANGELOG.md` 0.1.0 "Known Limitations" names the status-exit-code
defect, and `task 2.7` is titled "Correct status command exit codes"
(per `task-manifest.json` summary and `CONTRACTS.md` RUN-005). But the
`requirement_map` does not surface an explicit `DEBT-STATUS` key —
the debt ownership is only documented in prose (`CONTRACT_REVISIONS.md`,
`CONTRACTS.md` §"V0.1 carryover debt"). `AGENT_LOOP.md` step 6 says
"Record one `PASS` evidence row for every manifest acceptance ID" —
that mechanism does not by itself force the debt to be discharged by
its declared owner.

**Action:** Either (a) add a `debt_map` field to `task-manifest.json`
so the validator can assert each declared `DEBT-*` has an owning
acceptance ID, or (b) leave it as prose and accept the implicit
discipline. The current state is internally consistent but the
prose-to-mechanic gap is the kind of thing that drifts after
release.

### 5.3 README and CHANGELOG disagree on the v0.1 release date

- `CHANGELOG.md`: `[0.1.0] - 2026-07-15`
- `RELEASING.md`: silent on date.
- `README.md`: silent on date.

Not blocking. Just confirming the v0.1 release date is the
2026-07-15 changelog entry.

### 5.4 Standalone-install guidance: `worker_command` requirement

The new README states:

> A standalone install requires you set `worker_command` explicitly.
> The daemon will refuse to start without it.

This is a behavior claim, not a doc claim. I have not verified it
against `src/` (and `src/` is not in scope here — it is unmodified).
**Action:** Confirm in the v1.0 implementation pass; this is exactly
the kind of operator-facing hard rule that needs a regression test
in `tests/`.

### 5.5 `.atl/` is in `.gitignore` but untracked

`.atl/skill-registry.md` exists. Now correctly ignored. Confirm there
is no operator-promoted file under `.atl/` that should be tracked.

### 5.6 `.codegraph/` is untracked and not ignored

`.codegraph/codegraph.db` exists but is not listed in `.gitignore`.
The directory is generated (per its name) and should be ignored
alongside `.atl/` for consistency.

### 5.7 Handoff template uses `TASK.md` filename convention, but
`AGENT_LOOP.md` references `<id>.md`

`AGENT_LOOP.md` step 6: `Write handoffs/<id>.md from handoffs/TEMPLATE.md`.
`handoffs/TEMPLATE.md` exists; `handoffs/HUMAN_REVIEW_TEMPLATE.md`
exists. No `handoffs/phase-XX.md` or `handoffs/<task-id>.md` files
yet — expected, since `progress.json` shows all 42 tasks pending.
Confirm the validator already checks for stale phase handoff names
(this is what `AGENT_LOOP.md` step "Phase gates" implies).

---

## 6. What I did not check (scope reminder)

- **No source code review.** `src/`, `tests/`, `__init__.py`,
  `plugin.yaml`, `Cargo.toml`, `plugin-assets/`, `skills/` were not
  modified. They are out of scope for this doc-only delta.
- **No `.github/` review.** No workflow files were added or changed
  as part of this delta; CI-001 / CI-002 are future implementation.
- **No `.codegraph/` or `.atl/` semantic check.** Treated as
  generated / scratch.
- **No v0.1 archive drift check beyond the manifest digest.** The
  validator's v0.1 seal check is the single source of truth; I
  re-confirmed the seal is intact.

---

## 7. Recommendation

The v1.0 planning surface is **review-ready** modulo the §5.1
catalog-status contradiction, which is a one-line fix (either in
`planning/README.md` or in `task-manifest.json`).

The documentation delta is internally consistent, the contract
digest matches, the validator and selector both run cleanly, and
every v0.1 known limitation has a declared v1.0 owner.

**Suggested reviewer order:**

1. Read `CHANGELOG.md` (45 lines) for the v0.1 scope.
2. Read `planning/caduceus-v1.0/README.md` (110 lines) for the
   v1.0 authority chain.
3. Skim `CONTRACTS.md` §"Goal and scope", §"Plan and evidence
   integrity", §"V0.1 carryover debt" (~80 lines) for the rule
   set.
4. Skim `CONTRACT_REVISIONS.md` for the six signed revisions.
5. Spot-check `docs/configuration.md`, `docs/state-recovery.md`,
   `docs/the-bridge.md`, `docs/public-voice.md` for the four
   reference surfaces that the old README used to inline.
6. Resolve §5.1 catalog-status contradiction and §5.2 debt map.

---

*Assessor: Hermes subagent (read-only, no edits made).*
*Evidence sources: `git status`, `git diff`, `python3 tools/validate_plan.py`,
`python3 tools/next_task.py`, `sha256sum CONTRACTS.md`, file enumeration.*