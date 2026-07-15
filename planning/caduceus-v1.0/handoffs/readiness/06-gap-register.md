# Attachment 6 — Public Gap Register

Every gap surfaced by Attachments 1–5 is recorded here with
exactly one existing task/acceptance owner or one approved
deferral. Public promises that remain unowned or contradicted
are the focus of this register; satisfying **0.1-AC-06** means
that no row here has `Owner: —` and is also `Status: open`.

The register is the only authoritative index of "what is
missing between shipped behavior and the contract." Tasks
discover new gaps, but this register routes them, it does not
implement them.

## G-01 — Status exit codes (DEBT-STATUS)

- **What the operator sees today:** `caduceus status` always
  returns 0; `status --json` always renders, even on a missing
  state directory.
- **Contract:** `CONTRACTS.md` `RUN-005` — exit 0 on a clean
  render, exit 2 when state is missing, exit 1 when state or
  queue is corrupt.
- **Source of the gap:** `src/status.rs::report` does not bubble
  the diagnostic up to the CLI as a process exit code; today the
  CLI always calls `Ok(())` (see `src/cli.rs:147-161`).
- **Status:** open.
- **Owner:** Task 2.7 (`2.7-AC-01`).
- **Human review required?** No.

## G-02 — Config loader is a stub

- **What the operator sees today:** any cron tick or
  `caduceus run` fails with `"Config::load is implemented in
  Task 1.3"`.
- **Contract:** `CONTRACTS.md` `INSTALL-001` — fail-closed
  resolution chain, never silently fall through to host
  defaults; setup is the only path that may create a config.
- **Source of the gap:** `src/config.rs:350-357` is a
  deliberately-pinned stub; the production resolver lives
  behind the v1.0 contract surface.
- **Status:** open.
- **Owner:** Task 2.1 (`2.1-AC-01`).
- **Note:** Task 1.3 in the v0.1 plan owns the env-aware
  resolution chain; the v1.0 INSTALL-001 surface is the
  same code path, with the v1.0 contract pinned at the CLI
  boundary. Phase 02 Task 2.1 owns the v1.0 contract surface
  and reuses the v0.1 work where the rules align.

## G-03 — `worker_command` is not enforced for standalone installs

- **What the operator sees today:** the README claims the
  daemon "will refuse to start without it," but
  `Config::load` is the stub from G-02, so the rule is not
  enforced today.
- **Contract:** `CONTRACTS.md` `INSTALL-001` (a set
  `$CADUCEUS_CONFIG` is authoritative; missing or invalid
  content is an error; setup is the only path that may create
  a config).
- **Status:** open.
- **Owner:** Task 2.1.
- **Note:** Closes automatically when G-02 is fixed; the
  validator in `Config::load_with_paths` already rejects
  empty `worker_command` for the standalone path.

## G-04 — Single production worker adapter (`RUN-001`)

- **What the operator sees today:** the v0.1 cron tick invokes
  the bridge via a worker-supervisor; the bridge's
  `invoke_harness` calls `opencode` as a subprocess. The
  contract pins a single production adapter that the daemon
  owns; v0.1 already runs that path, but the v1.0 stable
  operation IDs, transcript cap, and result-schema validation
  are not yet on disk.
- **Contract:** `CONTRACTS.md` `RUN-001`.
- **Status:** open.
- **Owner:** Task 2.3 (`2.3-AC-01`).
- **Human review required?** No.

## G-05 — Worker deadline + descendant rediscovery (`RUN-002`)

- **What the operator sees today:** the supervisor enables
  the subreaper and reaps the worker PGID + PID; a hard
  deadline and explicit rediscovery of the descendant tree
  are not yet bounded.
- **Contract:** `CONTRACTS.md` `RUN-002`.
- **Status:** open.
- **Owner:** Task 2.4 (`2.4-AC-01`).
- **Human review required?** **Yes** — this is one of the
  four review-only tasks in the v1.0 plan.

## G-06 — Bounded transcripts (`RUN-003`)

- **What the operator sees today:** worker stderr is
  forwarded to a transcript file by the supervisor's
  background thread; there is no size cap and no failure
  surfacing.
- **Contract:** `CONTRACTS.md` `RUN-003`.
- **Status:** open.
- **Owner:** Task 2.5 (`2.5-AC-01`).
- **Human review required?** No.

## G-07 — Hardened Git + ephemeral credential transport (`RUN-004`)

- **What the operator sees today:** `src/worktree.rs::GitRunner`
  uses `OsString`; there is no NUL-delimited output guarantee
  and no ephemeral credential broker.
- **Contract:** `CONTRACTS.md` `RUN-004`.
- **Status:** open.
- **Owner:** Task 2.6 (`2.6-AC-01`).
- **Human review required?** No.

## G-08 — Transactional cron scheduling (`HERMES-001`)

- **What the operator sees today:** the adapter reconciles
  zero / one / >1 matches; it does not yet snapshot wrapper
  bytes + mode and re-list after create/update errors.
- **Contract:** `CONTRACTS.md` `HERMES-001`.
- **Status:** open.
- **Owner:** Task 2.2 (`2.2-AC-01`).
- **Human review required?** No.

## G-09 — Diagnosable host health (`HERMES-002`)

- **What the operator sees today:** `hermes caduceus doctor`
  prints binary / bridge / wrapper / cron presence; it does
  not yet check the contract-pinned categories
  (`host-capability-unavailable`, `gateway-inactive`,
  `config-incomplete`, `daemon-defect`) and does not check
  provider secret-name presence.
- **Contract:** `CONTRACTS.md` `HERMES-002`.
- **Status:** open.
- **Owner:** Task 2.2 (`2.2-AC-02`).
- **Human review required?** No.

## G-10 — SQLite runtime store (`STATE-001`)

- **What the operator sees today:** the v0.1 store is a
  JSON file under `<state_dir>/state.json` plus a
  `state_meta.json` sidecar. v1.0 is SQLite.
- **Contract:** `CONTRACTS.md` `STATE-001`.
- **Status:** open.
- **Owner:** Task 3.2 (`3.2-AC-01`).
- **Human review required?** No.

## G-11 — `migrate-state --to sqlite` (`STATE-002`)

- **What the operator sees today:** the shipped binary
  exposes `caduceus migrate-state --from <legacy.json>
  [--dry-run]`; the contract names `caduceus migrate-state
  --to sqlite` as the v1.0 form. Operator docs flag the
  shipped flag set as the currently supported command and
  flag `--to sqlite` as planned.
- **Contract:** `CONTRACTS.md` `STATE-002` + the
  v0.1 ↔ v1.0 cross-reference note recorded in
  `CONTRACT_REVISIONS.md`.
- **Status:** open.
- **Owner:** Task 3.3 (`3.3-AC-01`).
- **Human review required?** No.

## G-12 — `recover-state` command (`STATE-003`)

- **What the operator sees today:** there is no
  `caduceus recover-state` shipped; corrupt-state recovery
  is documentation only.
- **Contract:** `CONTRACTS.md` `STATE-003`.
- **Status:** open.
- **Owner:** Task 3.4 (`3.4-AC-01`).
- **Human review required?** **Yes** — one of the four
  review-only tasks.

## G-13 — Generations + retention (`STATE-004`)

- **What the operator sees today:** the queue has terminal
  `Failed` / `Skipped` phases; a reopened issue does not
  yet create a new generation, and retention is the v0.1
  default (no compaction).
- **Contract:** `CONTRACTS.md` `STATE-004`.
- **Status:** open.
- **Owner:** Tasks 3.5 (`3.5-AC-01`) and 3.6 (`3.6-AC-01`).
- **Human review required?** No.

## G-14 — Backup retention policy (DEBT-RETENTION)

- **What the operator sees today:** the v0.1 migrate path
  retains a backup but does not enforce a rotation policy
  or compaction cadence.
- **Contract:** `CONTRACTS.md` `STATE-004` retention half.
- **Status:** open.
- **Owner:** Task 3.6 (`3.6-AC-01`, owned alone).
- **Human review required?** No.

## G-15 — Atomic file installation primitive (DEBT-ATOMIC)

- **What the operator sees today:** the plugin adapter has
  its own atomic-install helper for the daemon binary
  (`__init__.py:_atomic_install_binary`); the daemon
  itself does not yet have a shared `install::atomic_write`
  primitive for state files.
- **Contract:** `CONTRACTS.md` `STATE-002` + `STATE-003`
  require atomic installation under the daemon lock.
- **Status:** open.
- **Owner:** Task 3.1 (the named "consolidate atomic file
  installation" task, owned alone).
- **Human review required?** No.

## G-16 — v0.1 MSRV packet is stale (DEBT-MSRV)

- **What the operator sees today:** the v0.1 packet
  `planning/caduceus-v0.1/tasks/9.2-…` and the v0.1
  archive `planning/caduceus-v0.1/archive/full-reviewed-plan.md`
  both still mention "Rust 1.75" in pre-CR-002 text. The
  current toolchain is Rust 1.97 (per `Cargo.toml`,
  `CONTRACT_REVISIONS.md` CR-002, and `CONTRACTS.md`
  `CONFIG-002`).
- **Contract:** `CONTRACTS.md` `CONFIG-002` pins MSRV 1.97.
- **Status:** open.
- **Owner:** Task 0.3 (`0.3-AC-01`, owned alone; v0.1
  archive remains byte-for-byte unchanged).
- **Human review required?** No.

## G-17 — Close-without-merge → NeedsAttention (`FINAL-001`)

- **What the operator sees today:** a PR closed without
  merge does not reliably transition the queue entry to
  `NeedsAttention`; v0.1 lacks the v1.0 stable operation
  IDs and remote reconciliation.
- **Contract:** `CONTRACTS.md` `FINAL-001` final paragraph
  (conflicting markers transition the generation to
  `NeedsAttention`).
- **Status:** open.
- **Owner:** Task 4.2 (`4.2-AC-01`).
- **Human review required?** No.

## G-18 — Concurrency + leases (`SCHED-001`)

- **What the operator sees today:** the daemon uses a
  whole-tick flock; there is no scheduler leadership, no
  per-issue leases, no fenced tokens, no backpressure.
- **Contract:** `CONTRACTS.md` `SCHED-001`.
- **Status:** open.
- **Owner:** Tasks 5.1 (`5.1-AC-01`) and 5.2 (`5.2-AC-01`).
- **Human review required?** No.

## G-19 — Circuit breakers (`SCHED-002`)

- **What the operator sees today:** the daemon has
  exponential backoff today (`tests/retry_test.rs`); the
  v1.0 provider/repository circuit breakers, half-open
  probes, and 24-hour max degraded age are not yet
  implemented.
- **Contract:** `CONTRACTS.md` `SCHED-002`.
- **Status:** open.
- **Owner:** Task 5.3 (`5.3-AC-01`).
- **Human review required?** No.

## G-20 — Daemon-owned bare mirrors (`REPO-001`)

- **What the operator sees today:** the daemon creates
  worktrees from the operator's checkout. v1.0 needs
  daemon-owned bare mirrors under mode-0700 storage.
- **Contract:** `CONTRACTS.md` `REPO-001`.
- **Status:** open.
- **Owner:** Task 5.4 (`5.4-AC-01`).
- **Human review required?** No.

## G-21 — `api_base` allowlist (GH-001 extension)

- **What the operator sees today:** `Config::load` accepts
  any HTTPS `api_base`. The v1.0 allowlist restricts the
  field to GitHub.com or GHES and forbids the
  `comment_forbidden_strings` substitute.
- **Contract:** `CONTRACTS.md` `GH-001` "Supported GitHub
  API endpoints".
- **Status:** open.
- **Owner:** Task 5.5 (`5.5-AC-01` + `5.5-AC-05`).
- **Human review required?** No.

## G-22 — Executor interface (`EXEC-001`)

- **What the operator sees today:** the daemon uses the
  trusted-host worker path; the executor abstraction and
  the OCI CLI executor are not yet introduced.
- **Contract:** `CONTRACTS.md` `EXEC-001`.
- **Status:** open.
- **Owner:** Tasks 6.1 (`6.1-AC-01`) and 6.2 (`6.2-AC-01`).
- **Human review required?** No.

## G-23 — OCI isolation defaults (`EXEC-002`)

- **What the operator sees today:** there is no OCI
  isolation; the daemon defaults to trusted-host.
- **Contract:** `CONTRACTS.md` `EXEC-002`.
- **Status:** open.
- **Owner:** Tasks 6.3 (`6.3-AC-01`) and 6.4 (`6.4-AC-01`).
- **Human review required?** **Yes** — one of the four
  review-only tasks.

## G-24 — Configuration schema v2 (`CONFIG-001`)

- **What the operator sees today:** the v0.1 config is the
  YAML under `caduceus:`; v1.0 needs the
  `worker_parallelism` default, executor mode, OCI
  settings, explicit repository scope, and circuit
  / retention settings.
- **Contract:** `CONTRACTS.md` `CONFIG-001`.
- **Status:** open.
- **Owner:** Task 3.7 (`3.7-AC-01`).
- **Human review required?** No.

## G-25 — CI matrix + fixtures (`CI-001`, `CI-002`, `CI-003`)

- **What the operator sees today:** the v0.1 CI gate is
  the local four-command sequence in `AGENTS.md`; there
  is no GitHub Actions workflow, no Wiremock GitHub
  fixture, no disposable local Git origin, no
  process-crash or release-binary fixture, no pinned
  Hermes-host fixture, and no Conventional Commits
  PR-time check.
- **Contract:** `CONTRACTS.md` `CI-001`..`CI-003`.
- **Status:** open.
- **Owner:** Tasks 1.1 (`1.1-AC-01`..`1.1-AC-05`),
  1.2 (`1.2-AC-01`), 1.3 (`1.3-AC-04`), 1.4
  (`1.4-AC-01`..`1.4-AC-10`).
- **Human review required?** No.

## G-26 — Full-system regression suite (`ACCEPT-001`..`ACCEPT-003`)

- **What the operator sees today:** the v0.1 test suite
  covers the surface that the v0.1 plan approved; the v1.0
  cross-subsystem regression matrix, real Hermes lifecycle,
  and installed-path canary are not yet built.
- **Contract:** `CONTRACTS.md` `ACCEPT-001`..`ACCEPT-003`.
- **Status:** open.
- **Owner:** Tasks 7.1 (`7.1-AC-01`), 7.2 (`7.2-AC-01`),
  7.3 (`7.3-AC-01`), 7.4 (`7.4-AC-01`), 7.5
  (`7.5-AC-01`, human review), 7.6 (`7.6-AC-01`).
- **Human review required?** **Yes** for 7.5 only.

## Gap summary

| Owner task | Open gaps | Review gate |
|---|---|---|
| 0.3 | 1 (G-16) | no |
| 1.1 | 1 (G-25 AC subset) | no |
| 1.2 | 1 (G-25 AC subset) | no |
| 1.3 | 1 (G-25 AC subset) | no |
| 1.4 | 1 (G-25 AC subset) | no |
| 2.1 | 2 (G-02, G-03) | no |
| 2.2 | 2 (G-08, G-09) | no |
| 2.3 | 1 (G-04) | no |
| 2.4 | 1 (G-05) | **yes** |
| 2.5 | 1 (G-06) | no |
| 2.6 | 1 (G-07) | no |
| 2.7 | 1 (G-01, DEBT-STATUS) | no |
| 3.1 | 1 (G-15, DEBT-ATOMIC) | no |
| 3.2 | 1 (G-10) | no |
| 3.3 | 1 (G-11) | no |
| 3.4 | 1 (G-12) | **yes** |
| 3.5 | 1 (G-13 generation half) | no |
| 3.6 | 2 (G-13 retention, G-14 DEBT-RETENTION) | no |
| 3.7 | 1 (G-24) | no |
| 4.1 | 1 (G-17 partial) | no |
| 4.2 | 1 (G-17) | no |
| 5.1 | 1 (G-18) | no |
| 5.2 | 1 (G-18) | no |
| 5.3 | 1 (G-19) | no |
| 5.4 | 1 (G-20) | no |
| 5.5 | 1 (G-21) | no |
| 6.1 | 1 (G-22) | no |
| 6.2 | 1 (G-22) | no |
| 6.3 | 1 (G-23) | no |
| 6.4 | 1 (G-23) | **yes** |
| 7.1 | 1 (G-26) | no |
| 7.2 | 1 (G-26) | no |
| 7.3 | 1 (G-26) | no |
| 7.4 | 1 (G-26) | no |
| 7.5 | 1 (G-26) | **yes** |
| 7.6 | 1 (G-26) | no |

Every open gap has exactly one named task/acceptance owner. No
gap is `Owner: —`; no public promise is unowned or
contradicted without a routing. Approved deferrals (none
today) would be recorded as a separate "Approved deferral"
table here; the public-voice plan is honest about which
planned work is in v1.0 and which is documented as out of
scope under "Explicit v1.x deferrals" in `CONTRACTS.md`.

## Reproduction

```bash
# Validate the requirement map and the v0.1 tree seal
python3 -B planning/caduceus-v1.0/tools/validate_plan.py

# Confirm gap routing lines up with the manifest
python3 -c "
import json
m = json.load(open('planning/caduceus-v1.0/task-manifest.json'))
for task in m['tasks']:
    for ac in task.get('acceptance_ids', []):
        print(ac, task['id'], task['title'])
" | sort

# Cross-check status exit-code path (G-01)
grep -nE "StatusDiagnostic::|process::exit" src/status.rs src/cli.rs

# Cross-check Config::load stub (G-02)
sed -n '348,360p' src/config.rs

# Cross-check StatusReporter returns Ok(()) in all cases (G-01)
sed -n '147,170p' src/cli.rs
```