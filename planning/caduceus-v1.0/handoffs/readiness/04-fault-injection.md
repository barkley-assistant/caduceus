# Attachment 4 — Fault Injection Matrix

For each fault category the operator or fixture may produce, this
attachment names the production surface that catches it today, the
test that exercises the surface, and the planned v1.0 owner when
the current behavior is incomplete. The matrix satisfies
`0.1-AC-04`.

A row marked `uncovered` is a gap; its Owner column names the
v1.0 task that must close the gap before Phase 02 implementation
begins.

## H — Hermes tool errors

| Fault | Today's behavior | Source | Test | Owner |
|---|---|---|---|---|
| `ctx.dispatch_tool("cronjob", …)` returns malformed JSON | `_runtime._coerce_jobs` returns `{}` (currently); the adapter rejects the empty registry as a hard error | `_runtime.py:_coerce_jobs`, `__init__.py:_cron_install` | `tests/hermes_plugin_test.py::test_cron_install_rejects_malformed_cron_response` | — |
| Hermes denies the call | `_runtime._dispatch` raises `RuntimeError`; the adapter's `_cli_cron_install` prints a redacted message and returns 1 | `__init__.py:_cron_install_cli`, `_runtime.py:_dispatch` | `tests/hermes_plugin_test.py::test_cron_install_rejects_denied_cron_response` | — |
| Hermes times out | `subprocess.run(...timeout=15)` raises `TimeoutExpired`; the adapter's `_run` helper maps it to a chat-friendly `RuntimeError` | `__init__.py:_run` | implicit via the `_run` helper | — |
| Hermes crashes (EOF) | `_coerce_jobs` returns `{}` after EOF is treated as a malformed response | `_runtime.py:_coerce_jobs` | `tests/hermes_plugin_test.py` | — |
| Duplicate cron jobs (foreign-name collision) | adapter raises `RuntimeError("multiple caduceus cron jobs found: …")` and exits 1 | `__init__.py:_cron_install` | `tests/hermes_plugin_test.py::test_cron_install_rejects_multiple_matches` | — |
| After create/update error or ambiguous outcome, exact state not achieved | adapter re-lists and reconciles, compensating wrapper and job to prior state; if exact rollback is impossible, returns `NeedsAttention` evidence | `__init__.py:_cron_install`, `HERMES-001` | unit-level; full transactional reconciliation in Task 2.2 | 2.2 (HERMES-001) |

## M — Malformed / timeout / side-effect outcomes

| Fault | Today's behavior | Source | Test | Owner |
|---|---|---|---|---|
| Bridge returns non-zero exit code | `tick::run_blocking` records the failure; the cron tick returns 1 per `RUN-005` (only for `Config` errors today) | `src/tick.rs::exit_code_for` | `tests/tick_test.rs`, `tests/failure_investigation_test.rs` | — |
| Bridge never returns (timeout) | `worker_supervisor` reaps the worker session with TERM → KILL | `src/main.rs:run_supervisor_mode` | `tests/worker_process_test.rs::test_*timeout*` | 2.4 (RUN-002 deadline) |
| Bridge produces invalid `worker-result.json` | finalize path rejects unknown top-level fields; the v1.0 size cap and validation rules are owned by `RUN-001` | `src/finalize.rs` | `tests/worker_result_test.rs` | 2.3 (RUN-001) |
| Worker outlives daemon (orphan) | `worker_supervisor` enables the subreaper on Linux and tracks the worker PGID + PID | `src/main.rs:run_supervisor_mode` | `tests/worker_parent_death_test.rs`, `tests/reaper_test.rs` | 2.4 |
| Bridge subprocess returns 0 but produced a NUL in `summary` or `commit_message` | v1.0 schema validation rejects NUL in required strings | `CONTRACTS.md` RUN-001 | `tests/worker_result_test.rs` | 2.3 |
| Bridge subprocess returns 0 with `pull_request_title` > 256 chars | rejected at validation | `CONTRACTS.md` RUN-001 | `tests/worker_result_test.rs` | 2.3 |
| Worker writes `worker-result.json` larger than 1 MiB | rejected at validation | `CONTRACTS.md` RUN-001 | `tests/worker_result_test.rs` | 2.3 |
| Worker writes a `summary` larger than 64 KiB | rejected at validation | `CONTRACTS.md` RUN-001 | `tests/worker_result_test.rs` | 2.3 |

## C — Configuration

| Fault | Today's behavior | Source | Test | Owner |
|---|---|---|---|---|
| `$CADUCEUS_CONFIG` set but file is missing or unreadable | `Config::load_from` returns `CaduceusError::Config`; cron tick fails | `src/config.rs::load_from` | `tests/config_resolution_test.rs::test_*missing*` | 2.1 (INSTALL-001) |
| `$HERMES_HOME` set to empty or relative path | rejected at `resolve_sources` | `src/config.rs::resolve_sources` | `tests/config_resolution_test.rs::test_*relative_hermes_home*` | 2.1 |
| Standalone install without `worker_command` | README claims the daemon refuses; today `Config::load` is a stub so the rule is not enforced | `src/config.rs:350-357` | uncovered | 2.1 |
| Token chain returns no value | `resolve_github_token` returns a structured error; `gh auth token` failure preserves the secret | `src/config.rs::resolve_token_chain` | `tests/token_test.rs::test_*missing*` | — |
| `api_base` is not GitHub.com or GHES | rejected at `Config::load` (positive allowlist) | `CONTRACTS.md` GH-001 final section | uncovered | 5.5 (5.5-AC-05) |
| `api_base` slips past via `comment_forbidden_strings` | rejected: GH-001 explicitly forbids the forbidden-string substitute | `CONTRACTS.md` GH-001 final section | uncovered | 5.5 |

## G — GitHub

| Fault | Today's behavior | Source | Test | Owner |
|---|---|---|---|---|
| 304 Not Modified with valid ETag | cached, treated as `Idle304`, exit 0 | `src/poll.rs`, `src/github.rs` | `tests/repository_poll_test.rs::test_etag_304_path` | — |
| 5xx from GitHub | exponential backoff per repo, no commit/push/PR/close | `src/github.rs` | `tests/rate_limit_test.rs`, `tests/retry_test.rs` | 5.3 (SCHED-002 circuit) |
| Rate limit hit | `next_allowed_poll_at` set, `SkippedRateLimited` outcome | `src/meta.rs` | `tests/rate_limit_test.rs` | 5.3 |
| PAT in `argv` or in Git config | refused by `RUN-004`; today the broker is not present | `CONTRACTS.md` RUN-004 | uncovered | 2.6 (RUN-004) |
| PR body contains a forbidden string | `finalize.rs` rejects the comment/PR body and surfaces the failure | `src/finalize.rs` | `tests/voice_rule_test.rs` | — |
| A label mutation triggers re-target after execution begins | v1.0 creates a new generation rather than mutating the active attempt | `CONTRACTS.md` STATE-004 | uncovered | 3.5 |

## Gt — Git

| Fault | Today's behavior | Source | Test | Owner |
|---|---|---|---|---|
| Path contains whitespace or non-UTF-8 bytes | the runner today uses `OsString` and surfaces raw bytes, but v1.0 hardening (NUL-delimited output, native path types, worktree containment) lands in 2.6 | `src/worktree.rs::GitRunner` | `tests/worktree_create_test.rs` | 2.6 (RUN-004) |
| Worktree path resolves outside the daemon-owned storage | refused by containment check after symlink resolution | `src/worktree.rs` | `tests/worktree_create_test.rs::test_*containment*` | 2.6 |
| Git hook or ambient config attempts to override daemon intent | refused; `RUN-004` final paragraph | `CONTRACTS.md` RUN-004 | uncovered | 2.6 |
| `git` times out (network slow on clone) | bounded with cancellable runner | `CONTRACTS.md` RUN-004 | `tests/worktree_create_test.rs` | 2.6 |
| Disposable local origin is unavailable during a CI test | `wiremock` substitute is used in test code | `tests/repository_poll_test.rs` | covered | 1.2 (CI-002 fixture) |

## W — Worker

| Fault | Today's behavior | Source | Test | Owner |
|---|---|---|---|---|
| Harness binary missing (`FileNotFoundError`) | bridge exits `EXIT_HARNESS_NOT_FOUND=127` | `plugin-assets/worker-bridge.py:296` | `tests/bridge_test.py::test_*harness_not_found*` | — |
| Harness unreachable (`OSError`) | bridge exits `EXIT_HARNESS_UNREACHABLE=126` | `plugin-assets/worker-bridge.py:302` | `tests/bridge_test.py::test_*harness_unreachable*` | — |
| Prompt file missing | bridge exits `EXIT_MISSING_PROMPT=2` | `plugin-assets/worker-bridge.py:verify_prompt` | `tests/bridge_test.py::test_verify_prompt_*` | — |
| Labels JSON malformed | bridge exits `EXIT_MALFORMED_LABELS=2` | `plugin-assets/worker-bridge.py:parse_labels` | `tests/bridge_test.py::test_parse_labels_*` | — |
| Required `CADUCEUS_*` env var missing | bridge exits `EXIT_MISSING_ENV=2` | `plugin-assets/worker-bridge.py:read_required_env` | `tests/bridge_test.py::test_bridge_validates_required_env` | — |
| Supervisor crash / EOF on stdin | worker session is killed; daemon sees the connection close | `src/main.rs:run_supervisor_mode` | `tests/worker_parent_death_test.rs` | 2.4 (RUN-002) |
| Worker process group has a stray descendant | subreaper + KILL-PGID path; v1.0 deadline + rediscovery are Task 2.4 | `CONTRACTS.md` RUN-002 | `tests/reaper_test.rs` | 2.4 |
| Transcript writer fails | v1.0 surfaces truncation / write failure (`RUN-003`); does not silently mark a run as success | `CONTRACTS.md` RUN-003 | uncovered | 2.5 |

## S — SQLite (planned)

| Fault | Today's behavior | Owner |
|---|---|---|
| Schema version newer than running daemon | rejected without mutation | 3.2 (STATE-001) |
| Invariant violation (e.g. claim without a corresponding queue entry) | transaction rolls back; surface is the planned v1.0 store | 3.2 |
| Migration loses state | v1.0 backup + import transaction + atomic activation (`STATE-002`) | 3.3 |
| Crash after each durable checkpoint | v1.0 idempotency keys + remote reconciliation; today the JSON store has the same shape but is not crash-safe across finalization | 4.1 (FINAL-001) |

## O — OCI (planned)

| Fault | Today's behavior | Owner |
|---|---|---|
| Image not pinned by digest | refused at executor config | 6.2 (EXEC-001) |
| Container has writable root | refused at runtime (read-only root, no-new-privileges) | 6.3 (EXEC-002) |
| `api_base` token reaches the container | refused: only per-run explicit secrets; `deny-by-default` OCI secret policy | 6.3 |
| Network is enabled by default | refused: network disabled unless an explicit profile enables it | 6.3 |
| Engine unavailability | surfaces `NeedsAttention`; never silently marks a run as success | 6.3 |
| Orphaned container from a previous daemon lifetime | bounded inventory + reconcile; orphan recovery in `EXEC-002` | 6.3 |

## Gw — Gateway

| Fault | Today's behavior | Source | Test | Owner |
|---|---|---|---|---|
| Hermes gateway not running | the cron job is registered but never fires; the operator sees no daemon activity | `hermes-integration.md` | uncovered (planned by ACCEPT-003) | 7.5 (ACCEPT-003) |
| Gateway restarts mid-tick | v0.1 has no transactional scheduler leadership; v1.0 introduces fenced leases | `CONTRACTS.md` SCHED-001 | uncovered | 5.1 |
| Cron tool not loaded in the calling Hermes session | `dispatch_tool("cronjob", …)` returns `Unknown tool: cronjob`; the wrapper file is written but the cron job is not. The v1.0 fix lands in `HERMES-001` | `_runtime.py:_dispatch` | `tests/hermes_plugin_test.py` exercises the dispatch boundary with a fake | 2.2 (HERMES-001) |
| Doctor cannot reach a network provider | doctor never calls a provider network; secret values are never inspected (HERMES-002) | `CONTRACTS.md` HERMES-002 | uncovered | 2.2 (HERMES-002) |

## P — Permissions

| Fault | Today's behavior | Source | Test | Owner |
|---|---|---|---|---|
| Cron job tries to read `$HERMES_HOME/caduceus-state/state.json` | state directory is mode 0700 | `__init__.py:_ensure_state_directories` | `tests/hermes_plugin_test.py::test_state_directory_is_mode_0700` | — |
| User bridge or wrapper is symlinked to a hostile path | `doctor` flags it and the adapter refuses; `STATE-002`-style install fails on symlink | `__init__.py:_cli_doctor`, `src/queue.rs` | `tests/hermes_plugin_test.py` | — |
| Atomic install of a binary fails partway | `_atomic_install_binary` is cross-fs-safe (copy + replace) and mode-preserving | `__init__.py:_atomic_install_binary` | `tests/hermes_plugin_test.py::test_setup_*` | — |
| File under `<state_dir>/runs/` is world-writable | refused at creation (`mode 0700` dir + `0600` private files) | `__init__.py:_ensure_state_directories`, `CONTRACTS.md` REPO-001 | covered for state dir; v1.0 mirrors this for transcripts / claim files | 5.4 (REPO-001) |
| Operator overwrites `state.json` directly | v0.1 has no in-place check; v1.0 forbids it via STATE-003 (and docs say so) | `CONTRACTS.md` STATE-003, `docs/state-recovery.md` | uncovered | 3.4 (STATE-003) |

## Reproduction

```bash
# Hermes tool errors
grep -nE "_coerce_jobs|def _dispatch" _runtime.py
grep -nE "_cron_install|_write_pulse_wrapper" __init__.py

# Configuration resolution
grep -nE "fn resolve_sources|fn resolve_token_chain|fn from_raw" src/config.rs

# GitHub + Git
grep -nE "Idle304|SkippedRateLimited|etag" src/github.rs src/poll.rs
grep -nE "impl GitRunner|fn run_git|fn worktree_containment" src/worktree.rs

# Worker
grep -nE "EXIT_|def invoke_harness|def read_required_env|def parse_labels" plugin-assets/worker-bridge.py
grep -nE "run_supervisor_mode|detach_session|ControlFrame" src/main.rs src/worker_supervisor.rs

# State directory mode
grep -nE "_ensure_state_directories|0o700|0o755" __init__.py _runtime.py
```