# Attachment 2 — Production-Path Reachability Map

Every public command the operator can invoke is walked from the
adapter / CLI entry point through to its production function and
the test that covers it. This catches stubs, dead paths, and
helper-only coverage before implementation begins.

## Convention

- **Entry** — the surface the operator types.
- **Adapter** — the first internal function that receives it.
- **Production function** — the Rust or Python function that
  performs the work.
- **Test** — the file + test name (or `—` when no test covers
  the path).
- **Result** — `covered`, `stub`, `helper-only`, or `dead`.

A row marked `dead` or `helper-only` is a discrepancy worth
routing; the row's Owner column names the v1.0 task that must
close the gap.

## A. CLI subcommands (`caduceus <sub>`)

| Entry | Adapter | Production function | Test | Result | Owner |
|---|---|---|---|---|---|
| `caduceus run` | `cli::run` (`src/cli.rs:106`) → `Command::Run` | `tick::run_blocking` (`src/tick.rs`), with hidden `__worker-supervisor` dispatch in `main.rs` | `tests/tick_test.rs`, `tests/cadence_test.rs`, `tests/dry_run_test.rs`, `tests/failure_investigation_test.rs` | covered | — |
| `caduceus status` | `cli::run` (`src/cli.rs:147`) → `Command::Status` | `status::report` (`src/status.rs:512`) | `tests/status_test.rs` | covered (but exit codes contradict `RUN-005`) | 2.7 |
| `caduceus status --json` | same as above; JSON branch | `status::report` JSON branch | `tests/status_test.rs` | covered | — |
| `caduceus worktree-gc [--older-than-days N] [--dry-run]` | `cli::run` → `cli::run_worktree_gc` (`src/cli.rs:171`) | `worktree::gc` (`src/worktree.rs`) | `tests/worktree_gc_test.rs` | covered | — |
| `caduceus queue reset <key> [--dry-run] [--force-finalization-reset]` | `cli::run` → `cli::run_queue_reset` (`src/cli.rs:219`) | `queue::StateStore::reset_entry` | `tests/queue_reset_cli_test.rs` | covered | — |
| `caduceus migrate-state --from <legacy.json> [--dry-run]` | `cli::run` → `cli::run_migrate_state` (`src/cli.rs:312`) | `migrate::run` (`src/migrate.rs`) | `tests/migration_test.rs` | covered | — |
| `caduceus migrate-state --to sqlite` | — | — | — | dead (not present in `src/cli.rs`) | 3.3 |
| bare `caduceus` (no args) | `cli::run` rewrites to `run` before Clap dispatch (`src/cli.rs:107-114`) | same as `caduceus run` | `tests/cadence_test.rs` | covered | — |

## B. Plugin adapter (`hermes caduceus <sub>`)

| Entry | Adapter | Production function | Test | Result | Owner |
|---|---|---|---|---|---|
| `hermes caduceus setup [--dry-run]` | `__init__.py:_cli_setup` (`__init__.py:337`) | `_build_daemon_binary` → `_atomic_install_binary` → `_ensure_state_directories` → `_seed_user_bridge` | `tests/hermes_plugin_test.py::test_setup_locks_rust_build_and_installs_binary_atomic`, `test_setup_idempotent`, `test_setup_preserves_user_bridge_and_emits_new_candidate` | covered | — |
| `hermes caduceus doctor` | `__init__.py:_cli_doctor` (`__init__.py:436`) | reads binary / bridge / wrapper / cron state | `tests/hermes_plugin_test.py` (cron presence + bridge + wrapper assertions) | covered | — |
| `hermes caduceus status` | `__init__.py:_cli_status` (`__init__.py:477`) | subprocess `<bin>/caduceus status` | implicit via `_cli_doctor` tests; no dedicated `pytest` case for `_cli_status` | helper-only | 1.2 (build a hermetic GitHub + Git fixture that drives status end-to-end) |
| `hermes caduceus cron-install [--dry-run]` | `__init__.py:_cli_cron_install` → `_cron_install` (`__init__.py:577`) | `_write_pulse_wrapper` + `_cron_job_registry` + `_cronjob_create/_update` | `tests/hermes_plugin_test.py::test_cron_install_*` (zero / one / multiple-match) | covered | — |
| `hermes caduceus cron-remove` | `__init__.py:_cli_cron_remove` (`__init__.py:659`) | removes cron job + pulse wrapper | `tests/hermes_plugin_test.py::test_cron_remove_*` | covered | — |
| `/caduceus-status` slash command | `__init__.py:_handle_caduceus_status` (`__init__.py:218`) | subprocess `<bin>/caduceus status --json` | `tests/hermes_plugin_test.py::test_status_slash_command_*` | covered | — |
| `caduceus:caduceus` skill | `__init__.py:register` (`__init__.py:171`) | `ctx.register_skill("caduceus", skills/caduceus/SKILL.md)` | `tests/hermes_plugin_test.py::test_skill_registered_as_caduceus_caduceus` | covered | — |
| `hermes caduceus recover-state` | — | — | — | dead (planned by Task 3.4) | 3.4 |

## C. Cronjob bridge (`_runtime.py`)

| Adapter | Production function | Test | Result | Owner |
|---|---|---|---|---|
| `cron_list_jobs()` | `_runtime.cron_list_jobs` → `_coerce_jobs` (`_runtime.py:_coerce_jobs`) | `tests/hermes_plugin_test.py` (cron install/removal uses `FakePluginContext` registry) | covered | — |
| `cron_create_job(...)` | `_runtime.cron_create_job` → `_dispatch` | same | covered | — |
| `cron_update_job(...)` | `_runtime.cron_update_job` → `_dispatch` | same | covered | — |
| `cron_remove_job(job_id)` | `_runtime.cron_remove_job` → `_dispatch` | same | covered | — |
| `install_dispatcher` / `reset_dispatcher` | `_runtime` module globals | `tests/hermes_plugin_test.py` exercises both | covered | — |

## D. Bridge contract (`plugin-assets/worker-bridge.py`)

| Entry | Adapter | Production function | Test | Result | Owner |
|---|---|---|---|---|---|
| `python3 worker-bridge.py` (CLI) | `main` (`plugin-assets/worker-bridge.py:264`) | `read_required_env` → `parse_labels` → `resolve_worktree` → `verify_prompt` → `invoke_harness` | `tests/bridge_test.py::test_main_*` (172 tests cover happy + each error path) | covered | — |
| `invoke_harness` (the operator-editable function) | `plugin-assets/worker-bridge.py:131` | `subprocess.run(argv, cwd=str(worktree))` | `tests/bridge_test.py::test_invoke_harness_*` | covered | — |
| `read_required_env(env)` | `plugin-assets/worker-bridge.py:185` | enumerates `REQUIRED_ENV_VARS` and exits `EXIT_MISSING_ENV` on missing | `tests/bridge_test.py::test_bridge_validates_required_env` | covered | — |
| `parse_labels(raw)` | `plugin-assets/worker-bridge.py:203` | `json.loads` + array-of-strings check | `tests/bridge_test.py::test_parse_labels_*` | covered | — |
| `verify_prompt(path)` | `plugin-assets/worker-bridge.py:231` | `Path.is_file()` check, exits `EXIT_MISSING_PROMPT` | `tests/bridge_test.py::test_verify_prompt_*` | covered | — |
| `resolve_worktree(env)` | `plugin-assets/worker-bridge.py:247` | resolves `CADUCEUS_WORKTREE_PATH` | `tests/bridge_test.py::test_resolve_worktree_*` | covered | — |

## E. Worker supervisor (production runtime path)

| Entry | Adapter | Production function | Test | Result | Owner |
|---|---|---|---|---|---|
| Hidden `caduceus __worker-supervisor --worktree … --run-id … --issue … --context-json … --transcript … --heartbeat … --timeout … -- <cmd>` | `main::run_supervisor_mode` (`src/main.rs:72`) | `worker_supervisor::detach_session` + framed protocol + worker spawn | `tests/worker_process_test.rs`, `tests/worker_parent_death_test.rs`, `tests/signal_test.rs`, `tests/reaper_test.rs` | covered (skeleton) | 2.3 (RUN-001), 2.4 (RUN-002 deadline), 2.5 (RUN-003 transcript) |
| Cron wrapper (`~/.hermes/scripts/caduceus-pulse.sh`) | `__init__.py:_write_pulse_wrapper` | `exec <bin> run "$@"` | `tests/hermes_plugin_test.py::test_cron_wrapper_*` | covered | — |

## F. State and queue

| Adapter | Production function | Test | Result | Owner |
|---|---|---|---|---|
| `StateStore::open(state_dir)` | `src/queue.rs::StateStore` | `tests/state_store_test.rs` | covered | — |
| `StateStore::reset_entry` | `src/queue.rs` | `tests/queue_reset_cli_test.rs` | covered | — |
| `StateStore::snapshot` | `src/queue.rs` | `tests/queue_model_test.rs` | covered | — |
| `DaemonLock::try_acquire` | `src/queue.rs::DaemonLock` | `tests/daemon_lock_test.rs` | covered | — |
| `MetaStore` | `src/meta.rs` | `tests/meta_test.rs` | covered | — |
| `migrate::run` | `src/migrate.rs` | `tests/migration_test.rs` | covered | — |
| SQLite store | — | — | dead (planned by Task 3.2) | 3.2 |

## G. GitHub + Git

| Adapter | Production function | Test | Result | Owner |
|---|---|---|---|---|
| `GithubClient` (typed HTTP, ETag cache) | `src/github.rs` | `tests/github_client_test.rs`, `tests/repository_poll_test.rs`, `tests/rate_limit_test.rs` | covered | — |
| `GitRunner` (worktree containment, NUL-delimited) | `src/worktree.rs::GitRunner` | `tests/worktree_create_test.rs`, `tests/worktree_remove_test.rs`, `tests/worktree_gc_test.rs` | covered | — |
| `Config::resolve_github_token` | `src/config.rs` | `tests/token_test.rs` | covered | — |
| `api_base` GHES allowlist | — | — | dead (planned by Task 5.5, 5.5-AC-05) | 5.5 |
| Ephemeral credential broker | — | — | dead (planned by Task 2.6, RUN-004) | 2.6 |

## H. CI and release (planned by Phase 01)

| Capability | State | Owner |
|---|---|---|
| GitHub Actions matrix on PR + push to `main` (`CI-001`) | dead | 1.1 |
| Wiremock GitHub + disposable local Git origin fixture (`CI-002`) | dead | 1.2 |
| Process-crash + release-binary fixture (`CI-002`) | dead | 1.3 |
| Pinned Hermes-host fixture (`CI-002`, `ACCEPT-003`) | dead | 1.4 |
| Conventional Commits PR check (`CI-003`) | dead | 1.1 |

## Reproduction

```bash
# CLI surface
grep -nE "Command::|fn run_(queue_reset|worktree_gc|migrate_state)" src/cli.rs

# Plugin surface
grep -nE "_cli_(setup|doctor|status|cron_install|cron_remove)|def register" __init__.py

# Cronjob bridge
grep -nE "def cron_|def _coerce_jobs|def install_dispatcher" _runtime.py

# Bridge contract
grep -nE "def main|def invoke_harness|def read_required_env|def parse_labels|def verify_prompt|def resolve_worktree|REQUIRED_ENV_VARS" plugin-assets/worker-bridge.py

# Worker supervisor
grep -nE "run_supervisor_mode|detach_session|encode_frame|ControlFrame" src/main.rs src/worker_supervisor.rs

# State + queue
grep -nE "impl StateStore|impl DaemonLock|impl MetaStore|pub fn reset_entry" src/queue.rs src/meta.rs

# GitHub + Git
grep -nE "impl GithubClient|impl GitRunner|fn resolve_github_token" src/github.rs src/worktree.rs src/config.rs
```