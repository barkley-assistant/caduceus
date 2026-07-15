# Attachment 1 — Public Capability Inventory

Every public capability surfaced in the operator-facing docs and code
is classified by current state. The inventory is the source of truth
for Phase 00 routing; later attachments reuse its rows.

The "Source" column points to the exact file and line range that
proves the classification. The "Owner" column names the v1.0 task
that must discharge the gap (if any) before the capability is
considered `working-production` under the v1.0 contract.

## A. CLI subcommands (`caduceus <sub>`)

| Capability | State | Source | Owner |
|---|---|---|---|
| `caduceus run` (default, bare invocation rewritten to `run`) | `working-production` | `src/cli.rs:106-146`, `src/main.rs:27-54` | — |
| `caduceus status [--json]` | `contradicted` | `src/cli.rs:147-161`, `src/status.rs:512-535`; **does not return the contract-pinned exit codes (0 / 2 / 1)** for `no_state` and `corrupt_*` paths | 2.7 (`DEBT-STATUS`) |
| `caduceus worktree-gc [--older-than-days N] [--dry-run]` | `working-production` | `src/cli.rs:171-217`, `src/worktree.rs`, `tests/worktree_gc_test.rs` | — |
| `caduceus queue reset <owner/repo#N> [--dry-run] [--force-finalization-reset]` | `working-production` | `src/cli.rs:219-310`, `tests/queue_reset_cli_test.rs` | — |
| `caduceus migrate-state --from <legacy.json> [--dry-run]` | `working-production` | `src/cli.rs:312-343`, `src/migrate.rs`, `tests/migration_test.rs` | — |
| `caduceus migrate-state --to sqlite` | `planned` | `CONTRACTS.md` STATE-002, Task 3.3 | 3.3 |
| `caduceus status --json` (chat-friendly summary) | `working-production` | `__init__.py:_handle_caduceus_status`, `__init__.py:_format_status_for_chat` | — |

## B. Plugin subcommands (`hermes caduceus <sub>`)

| Capability | State | Source | Owner |
|---|---|---|---|
| `hermes caduceus setup [--dry-run]` | `working-production` | `__init__.py:_cli_setup`, `tests/hermes_plugin_test.py` (locked build + atomic binary placement) | — |
| `hermes caduceus doctor` | `working-production` | `__init__.py:_cli_doctor`, prints binary/bridge/wrapper/cron state | — |
| `hermes caduceus status` | `working-production` | `__init__.py:_cli_status`, delegates to `<bin>/caduceus status` | — |
| `hermes caduceus cron-install [--dry-run]` | `working-production` | `__init__.py:_cron_install` + `_runtime.py:cron_*`; reconciles zero/one/>1 matches via `ctx.dispatch_tool("cronjob", …)` | — |
| `hermes caduceus cron-remove` | `working-production` | `__init__.py:_cli_cron_remove` | — |
| `/caduceus-status` slash command | `working-production` | `__init__.py:_handle_caduceus_status`, registered via `ctx.register_command` | — |
| `caduceus:caduceus` skill | `working-production` | `__init__.py:register`, `skills/caduceus/SKILL.md` | — |
| `hermes caduceus recover-state` (metadata repair) | `planned` | `CONTRACTS.md` STATE-003, Task 3.4 (human review) | 3.4 |

## C. Plugin manifest fields (`plugin.yaml`)

| Field | State | Source |
|---|---|---|
| `manifest_version`, `name`, `version`, `description`, `author`, `kind: standalone` | `working-production` | `plugin.yaml` (allowlist pinned by `tests/hermes_plugin_test.py`) |
| `requires_env: []`, `provides_tools: []`, `provides_hooks: []` | `working-production` | `plugin.yaml`; declared-empty list keeps the no-required-env contract obvious |
| Legacy `files`, `binaries`, `hooks`, `config`, `cron_profiles`, `profile_section` | `not-shipped` | negative fixture `tests/fixtures/negative_plugin.yaml` enforces rejection |

## D. Bridge contract (`plugin-assets/worker-bridge.py`, user-owned at `~/.hermes/caduceus/worker-bridge.py`)

| Capability | State | Source | Owner |
|---|---|---|---|
| Reads required `CADUCEUS_*` env vars (`ISSUE_NUMBER`, `ISSUE_TITLE`, `ISSUE_BODY`, `ISSUE_REPO`, `ISSUE_LABELS_JSON`, `CONTEXT_JSON`, `WORKTREE_PATH`, `RUN_ID`, `BRANCH_NAME`) | `working-production` | `plugin-assets/worker-bridge.py:REQUIRED_ENV_VARS`, `tests/bridge_test.py::test_bridge_validates_required_env` | — |
| Validates `CADUCEUS_ISSUE_LABELS_JSON` as a JSON array of strings | `working-production` | `plugin-assets/worker-bridge.py:parse_labels`, `tests/bridge_test.py` | — |
| Resolves worktree path and verifies the rendered prompt | `working-production` | `plugin-assets/worker-bridge.py:resolve_worktree`/`verify_prompt`, tests cover both | — |
| Invokes harness as an argument array (`opencode run --agent gentle-orchestrator -f <prompt>`) | `integrated-not-proven` | `plugin-assets/worker-bridge.py:invoke_harness` is the reference; the production worker adapter (`src/worker.rs` + `src/worker_supervisor.rs`) is the production path that actually launches the bridge | 2.3 (RUN-001 single production worker adapter) |
| Writes `<worktree>/worker-result.json` with status / summary / commit_message / pull_request_title / artifacts | `planned` | `CONTRACTS.md` RUN-001 pins the JSON shape and limits; bridge currently relies on the harness | 2.3 (RUN-001) |
| Exits `EXIT_OK=0`, `EXIT_MISSING_ENV=2`, `EXIT_HARNESS_NOT_FOUND=127`, `EXIT_HARNESS_UNREACHABLE=126` | `working-production` | `plugin-assets/worker-bridge.py:EXIT_*`; `tests/bridge_test.py` covers all | — |
| User-owned copy is preserved across setup; template drift writes a sibling `.new` candidate | `working-production` | `__init__.py:_seed_user_bridge`, `tests/hermes_plugin_test.py` | — |

## E. Configuration surface

| Capability | State | Source | Owner |
|---|---|---|---|
| `$CADUCEUS_CONFIG` (explicit path) | `working-production` | `src/config.rs:resolve_sources` (Task 1.3 owns the resolution chain) | — |
| `$HERMES_HOME/config.yaml` → `caduceus:` section | `working-production` | `src/config.rs:load_from`, `tests/config_resolution_test.rs` | — |
| `~/.config/caduceus/config.yaml` (standalone) | `working-production` | same path | — |
| `Config::load()` (full env-aware chain, fail-closed) | `stub` | `src/config.rs:350-357` returns `"Config::load is implemented in Task 1.3"`; cron tick cannot run | 2.1 (INSTALL-001) |
| `worker_command` field (standalone install requirement) | `contradicted` | README claims the daemon "will refuse to start without it"; `Config::load` is the stub above, so this rule is not enforced today | 2.1 |

## F. Worker supervisor (production runtime)

| Capability | State | Source | Owner |
|---|---|---|---|
| Detaches into a fresh Unix session, sends `READY {pgid}` frame | `working-production` | `src/main.rs:run_supervisor_mode`, `src/worker_supervisor.rs` | — |
| Awaits daemon `ACK`, then runs worker under `process_group(0)` | `working-production` | `src/main.rs:run_supervisor_mode`, `tests/worker_parent_death_test.rs` | — |
| Forwards worker stderr to transcript via background thread | `working-production` | `src/main.rs:run_supervisor_mode`, `tests/worker_process_test.rs` | — |
| Forwards `TERM` and `KILL` frames; cleans up worker PGID + PID | `working-production` | `src/main.rs:run_supervisor_mode` | — |
| Bounded transcript writer (`RUN-003`, 1 MiB cap) | `planned` | `CONTRACTS.md` RUN-003 | 2.5 |
| Hard deadline + descendant rediscovery (`RUN-002`) | `integrated-not-proven` | supervisor path exists; deadline + rediscovery are Task 2.4 | 2.4 (human review) |
| Hardened Git runner with NUL-delimited output + worktree containment (`RUN-004`) | `integrated-not-proven` | `src/worktree.rs::GitRunner` exists; the runner is a worker-supervisor path today, hardening is Task 2.6 | 2.6 |
| Ephemeral credential broker for GitHub PAT (`RUN-004`) | `planned` | `CONTRACTS.md` RUN-004 final paragraph | 2.6 |

## G. State and queue

| Capability | State | Source | Owner |
|---|---|---|---|
| Whole-tick `DaemonLock` (flock) | `working-production` | `src/queue.rs::DaemonLock`, `tests/daemon_lock_test.rs` | — |
| Per-issue claim files under `<state_dir>/claims/` | `working-production` | `src/queue.rs`, `tests/claim_test.rs` | — |
| Phases: Queued → InProgress → AwaitingReview → Done, with terminal Failed/Skipped | `working-production` | `src/queue.rs::Phase`, `tests/queue_model_test.rs` | — |
| Finalization checkpoints (`ResultValidated → Committed → Pushed → PrCreated → Commented → AwaitingReview → Done`) | `integrated-not-proven` | v0.1 already persists `FinalizationCheckpoint`; the v1.0 stable operation IDs + idempotency keys are Task 4.1 | 4.1 |
| SQLite state store (`STATE-001`) | `planned` | `CONTRACTS.md` STATE-001 | 3.2 |
| JSON-only import/export/backup format (`STATE-001`) | `working-production` | `src/migrate.rs`, `tests/migration_test.rs` | — |
| Backup retention + compaction (`DEBT-RETENTION`) | `planned` | `CONTRACTS.md` STATE-004, Task 3.6 | 3.6 |
| Generations + reprocess (`STATE-004`) | `planned` | Task 3.5 | 3.5 |
| `recover-state` command (`STATE-003`) | `planned` | Task 3.4 (human review) | 3.4 |

## H. GitHub integration

| Capability | State | Source | Owner |
|---|---|---|---|
| ETag-aware 304 polling per repo | `working-production` | `src/poll.rs`, `src/github.rs`, `tests/github_client_test.rs`, `tests/repository_poll_test.rs` | — |
| Fine-grained PAT authentication | `working-production` | `src/config.rs::resolve_github_token`, `tests/token_test.rs` | — |
| `api_base` allowlist (GitHub.com + GHES only) | `planned` | `CONTRACTS.md` GH-001 final section, Task 5.5 (5.5-AC-05) | 5.5 |
| Discovery is bounded incremental polling, may use ETags | `working-production` | `src/poll.rs` | — |
| Tokens are never exposed to workers / transcripts / Git / public GitHub text | `integrated-not-proven` | daemon-side allowlist + `DENIED_ENV_VARS` cover the runtime path; the worker-supervisor transport in `RUN-004` is Task 2.6 | 2.6 |

## I. Finalization pipeline (commit → push → PR → comment → close)

| Capability | State | Source | Owner |
|---|---|---|---|
| Commit on daemon-owned branch, push via hardened runner | `working-production` | `src/finalize.rs`, `tests/commit_test.rs`, `tests/push_test.rs` | — |
| Find-or-create PR; idempotent comment | `working-production` | `src/finalize.rs`, `tests/pr_test.rs`, `tests/issue_close_test.rs` | — |
| Public-voice rule on outbound comment + PR body | `working-production` | `src/finalize.rs`, `tests/pr_body_test.rs`, `tests/voice_rule_test.rs` | — |
| Investigation tickets skip commit/push/PR | `working-production` | `src/finalize.rs`, `tests/failure_investigation_test.rs` | — |
| Reopen / retarget / reprocess / merge / closed-without-merge | `planned` | `CONTRACTS.md` STATE-004 + FINAL-001; Task 3.5 (reopen/reprocess), Task 4.1 (idempotent checkpoints), Task 4.2 (close-without-merge → NeedsAttention) | 3.5, 4.1, 4.2 |
| Human merge lifecycle (`FINAL-002`, no auto-merge) | `working-production` | `src/finalize.rs` leaves the issue open; documented in `README.md` | — |

## J. Scheduling, repositories, and isolation

| Capability | State | Source | Owner |
|---|---|---|---|
| Single-host concurrency model (`SCHED-001`) | `planned` | `CONTRACTS.md` SCHED-001, Task 5.1 + 5.2 | 5.1, 5.2 |
| Bounded exponential backoff + circuit breakers (`SCHED-002`) | `planned` | Task 5.3 | 5.3 |
| Daemon-owned bare mirrors + disposable worktrees (`REPO-001`) | `planned` | Task 5.4 | 5.4 |
| OCI CLI executor (`EXEC-001`) | `planned` | Task 6.1 + 6.2 | 6.1, 6.2 |
| OCI isolation defaults + boundary (`EXEC-002`) | `planned` | Task 6.3, verified by 6.4 (human review) | 6.3, 6.4 |
| Trusted-host executor (`EXEC-001`) | `planned` | Task 6.1 (opt-in reduced containment) | 6.1 |

## K. CI and release

| Capability | State | Source | Owner |
|---|---|---|---|
| Local Rust: `cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked --all-targets` | `working-production` | run on every commit pre-push per `AGENTS.md` and `CONTRIBUTING.md` | — |
| Local Python: `pytest -q tests/hermes_plugin_test.py tests/bridge_test.py` | `working-production` | `tests/hermes_plugin_test.py`, `tests/bridge_test.py` (172 tests) | — |
| GitHub Actions matrix (`CI-001`) | `planned` | Task 1.1 (1.1-AC-01..04) | 1.1 |
| Wiremock GitHub + disposable local Git origin fixtures (`CI-002`) | `planned` | Task 1.2 (1.2-AC-01) | 1.2 |
| Process-crash + release-binary fixtures (`CI-002`) | `planned` | Task 1.3 (1.3-AC-04) | 1.3 |
| Pinned Hermes-host fixture (`CI-002`, ACCEPT-003) | `planned` | Task 1.4 (1.4-AC-01..10) | 1.4 |
| Conventional Commits policy (`CI-003`) | `planned` | Task 1.1 (1.1-AC-05, PR-time check) | 1.1 |
| Full-system regression suite (`ACCEPT-001`) | `planned` | Task 7.1 + 7.2 | 7.1, 7.2 |
| Real Hermes lifecycle (`ACCEPT-002`) | `planned` | Task 7.3 | 7.3 |
| Installed-path truth (`ACCEPT-003`) | `planned` | Task 7.5 (human review) | 7.5 |
| Operator documentation publish (`ACCEPT-002` second half) | `planned` | Task 7.4 | 7.4 |

## L. Operator-facing documents

| Capability | State | Source |
|---|---|---|
| `README.md` (front door, SOUL voice) | `working-production` | `README.md` |
| `MIGRATION.md` (operator migration runbook) | `working-production` | `MIGRATION.md` |
| `CONTRIBUTING.md` | `working-production` | `CONTRIBUTING.md` |
| `AGENTS.md` (agent + human contract) | `working-production` | `AGENTS.md` |
| `RELEASING.md`, `SECURITY.md`, `CHANGELOG.md` | `working-production` | repo root |
| `docs/installation.md` | `working-production` | `docs/installation.md` |
| `docs/configuration.md` | `working-production` | `docs/configuration.md` |
| `docs/the-bridge.md` | `working-production` | `docs/the-bridge.md` |
| `docs/state-recovery.md` | `working-production` | `docs/state-recovery.md` |
| `docs/public-voice.md` | `working-production` | `docs/public-voice.md` |
| `docs/architecture.md` | `working-production` | `docs/architecture.md` |
| `docs/plugin-lifecycle.md` | `working-production` | `docs/plugin-lifecycle.md` |
| `docs/hermes-integration.md` | `working-production` | `docs/hermes-integration.md` |
| `docs/troubleshooting.md` | `working-production` | `docs/troubleshooting.md` |
| `docs/faq.md` | `working-production` | `docs/faq.md` |

## M. Quality audit (`QUALITY-001`)

The QUALITY-001 surface rule says shipped hooks, scripts, assets,
skills, generated text, comments, errors, manifest fields, and
command paths must contain no `todo!()` / `unimplemented!()` /
deliberate stub / fake-only hook / dev-only manifest field.

A targeted scan of `src/`, `__init__.py`, `_runtime.py`,
`plugin-assets/`, `skills/caduceus/SKILL.md`, and `plugin.yaml`
finds **zero hits** for `todo!()` or `unimplemented!()` (verified
with `grep -rnE "todo!\(\)|unimplemented!\(\)" src/ __init__.py
_runtime.py plugin-assets/`).

The only acknowledged production stub is `Config::load` (above),
which is a contract surface to be completed in Task 2.1; the error
message names the owning task explicitly. No `unimplemented!()`
markers exist anywhere in production code.

## N. Documentation drift closed during v1.0 rework

These were flagged by the v1.0 rework assessor and have been
resolved before Phase 01 begins (closed, not re-flagged):

- `--from` vs `--to sqlite` migrate command split — closed in
  `CONTRACT_REVISIONS.md` §"v0.1 ↔ v1.0 migration-command split".
- `CADUCEUS_CONTEXT_JSON` schema — closed in
  `docs/the-bridge.md` §"The `CADUCEUS_CONTEXT_JSON` Schema".
- `LICENSE` reference in README — file present at `LICENSE` (MIT).

## Reproduction

Every row above is reproducible from this command set:

```bash
# CLI subcommands
grep -nE "pub enum Command|pub enum QueueAction" src/cli.rs
grep -nE "fn run_|process::exit" src/cli.rs src/main.rs

# Plugin subcommands + registration
grep -nE "register_(skill|command|cli_command)|_cli_(setup|doctor|status|cron_install|cron_remove)" __init__.py

# Bridge contract
grep -nE "REQUIRED_ENV_VARS|EXIT_|def invoke_harness|parse_labels|verify_prompt" plugin-assets/worker-bridge.py

# Configuration loader state
sed -n '340,360p' src/config.rs

# Status diagnostic surface
sed -n '500,540p' src/status.rs

# Worker supervisor skeleton
sed -n '70,180p' src/main.rs

# Quality scan
grep -rnE "todo!\(\)|unimplemented!\(\)" src/ __init__.py _runtime.py plugin-assets/
```