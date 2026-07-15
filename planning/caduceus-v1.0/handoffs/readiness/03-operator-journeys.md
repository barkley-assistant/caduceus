# Attachment 3 — Operator Journey Matrix

Every operator journey the user-facing docs and code promise. The
matrix is the operator's mental model of the install → run →
recover → uninstall flow. Each row is reproducible from the
shipping code or, when the journey is planned for v1.0, from the
task that owns it.

The "Reality" column records what the operator experiences today
versus what the contract promises. Rows where Reality diverges
from the promise are recorded with an explicit Owner and a single
acceptance ID that closes the gap.

## J-01 — Install from repository (Hermes)

- **Operator action:** `hermes plugins install barkley-assistant/caduceus --enable`
- **Adapter:** Hermes CLI; copies plugin tree into
  `$HERMES_HOME/plugins/caduceus/` (per `tests/hermes_plugin_test.py::test_install_copies_plugin_tree_into_hermes_home`)
- **Reality:** `working-production`. The plugin tree includes
  `plugin.yaml`, `__init__.py`, `skills/caduceus/SKILL.md`,
  `plugin-assets/worker-bridge.py`, `plugin-assets/caduceus-pulse.sh`,
  `Cargo.toml`, `Cargo.lock`, `src/`.
- **Owner:** —

## J-02 — Discovery + enablement

- **Operator action:** implicit; `hermes plugins enable caduceus` after install
- **Reality:** `working-production`. `__init__.py:register` is
  idempotent and stdlib-only.

## J-03 — First-time setup (build + bridge seeding)

- **Operator action:** `hermes caduceus setup`
- **Adapter:** `__init__.py:_cli_setup` (J-03 →
  `cargo build --release --locked --manifest-path <root>/Cargo.toml`
  → atomic install of `<plugin>/bin/caduceus` (mode 0755) → seed
  user bridge from `plugin-assets/worker-bridge.py` template)
- **Reality:** `working-production`. Covered by
  `test_setup_locks_rust_build_and_installs_binary_atomic`,
  `test_setup_idempotent`, `test_setup_preserves_user_bridge_and_emits_new_candidate`.
- **Note:** setup never overwrites the user bridge; if the
  shipped template changes, a `.new` candidate is written
  alongside.
- **Owner:** —

## J-04 — Schedule

- **Operator action:** `hermes caduceus cron-install`
- **Adapter:** `__init__.py:_cron_install` →
  `_write_pulse_wrapper(binary)` → reconcile cron registry via
  `_runtime.cron_list_jobs()` → create or update one cron job
  named `caduceus` with schedule `every 2m`, `no_agent=True`.
- **Reality:** `working-production`. Zero / one / >1 matches each
  have a dedicated test.
- **Note:** the wrapper contains `exec <absolute-binary-path> run
  "$@"` so the cron process is replaced by the daemon, not forked
  from a shell.
- **Owner:** —

## J-05 — Doctor

- **Operator action:** `hermes caduceus doctor`
- **Adapter:** `__init__.py:_cli_doctor`
- **Reality:** `working-production` for binary presence, bridge
  presence, wrapper presence, cron job presence (each printed with
  its exact path / id). Operator-friendly lifecycle hint table is
  always emitted.
- **Note:** Doctor today does not check the contract-pinned
  categories (`host-capability-unavailable`, `gateway-inactive`,
  `config-incomplete`, `daemon-defect`) or the configuration +
  provider secret presence required by `HERMES-002`. Those are
  owned by Task 2.2.
- **Owner:** 2.2 (HERMES-002)

## J-06 — Status (binary path)

- **Operator action:** `hermes caduceus status` (or
  `/caduceus-status`)
- **Reality:** `working-production` for rendering; **contradicted**
  for exit codes (the CLI returns 0 even when state is missing,
  where the contract requires 2; corrupt-state paths are not
  classified per `RUN-005`).
- **Owner:** 2.7 (DEBT-STATUS, RUN-005)

## J-07 — Status (chat-friendly summary)

- **Operator action:** `/caduceus-status`
- **Adapter:** `__init__.py:_handle_caduceus_status` →
  `_format_status_for_chat`
- **Reality:** `working-production` for the JSON-parse-and-format
  path. Surfaces `caduceus <version> — last tick: …` with queue
  phase counts, next head, and rate-limit summary.
- **Owner:** —

## J-08 — Manual run (foreground tick)

- **Operator action:** `caduceus run` (or bare `caduceus`)
- **Reality:** `integrated-not-proven`. `cli::run` rewrites a bare
  invocation to `run`, then calls `tick::run_blocking`, which
  invokes `Config::load()`. `Config::load` is a stub that returns
  `"Config::load is implemented in Task 1.3"`. So a manual run
  fails today with a contract-pinned error.
- **Owner:** 2.1 (INSTALL-001 — Task 1.3 will also close the same
  config surface, but the cron tick path is gated on the v1.0
  resolution chain shipped by Task 2.1)

## J-09 — Scheduled run (cron-driven tick)

- **Operator action:** the `caduceus` cron job fires; the wrapper
  executes `exec <bin> run`
- **Reality:** Same blocker as J-08: depends on `Config::load`.
- **Owner:** 2.1

## J-10 — Issue → PR (the canonical end-to-end journey)

- **Operator action:** label an issue `🤖 auto-fix`; wait two
  minutes; observe branch / push / PR / comment
- **Reality:** `contradicted` — the scheduler polls and the
  finalize path is fully implemented (`src/finalize.rs`,
  `tests/commit_test.rs`, `tests/push_test.rs`, `tests/pr_test.rs`,
  `tests/pr_body_test.rs`, `tests/issue_close_test.rs`), but the
  cron tick cannot fire today because `Config::load` is a stub.
- **Owner:** 2.1 (unblocks the cron tick); 4.1 (adds idempotent
  finalization checkpoints so the journey is crash-safe).

## J-11 — Restart the daemon / gateway

- **Operator action:** `systemctl restart hermes-gateway` (or
  equivalent) when the operator wants the cron job to resume
  firing after a host event
- **Reality:** `integrated-not-proven`. The Hermes gateway is the
  cron delivery surface today; the planned `ACCEPT-003` installed-
  path truth test (Task 7.5) covers restart with an external
  harness.
- **Owner:** 7.5 (ACCEPT-003, human review)

## J-12 — Merge the PR

- **Operator action:** review the PR opened by Caduceus; click
  `Merge`
- **Reality:** `working-production`. The finalize path leaves the
  issue open and the queue entry in `AwaitingReview`. There is no
  auto-merge. The closing keyword or explicit reconciliation
  transitions the generation to `Done`.
- **Owner:** —

## J-13 — Reject / close without merge

- **Operator action:** close the PR without merging
- **Reality:** `contradicted` for v1.0. Today the daemon does not
  reliably transition the queue entry to `NeedsAttention` when the
  PR is closed without merge; the v1.0 stable operation IDs and
  remote reconciliation land in Tasks 4.1 + 4.2.
- **Owner:** 4.1 (FINAL-001 idempotent checkpoints), 4.2
  (FINAL-001 conflicting markers → NeedsAttention)

## J-14 — Source update

- **Operator action:** `hermes plugins update caduceus` then
  `hermes caduceus setup`
- **Reality:** `working-production`. Re-clones the plugin tree,
  leaves user bridge / state / config alone, rebuilds the binary.
  Tested by `tests/hermes_plugin_test.py::test_source_update_*`.
- **Owner:** —

## J-15 — Migrate from v0 cron processor

- **Operator action:** `caduceus migrate-state --from <legacy.json> [--dry-run]`
- **Reality:** `working-production` for the shipped v0.1 form
  (`--from <legacy.json>`). The v1.0 form `caduceus migrate-state
  --to sqlite` is **planned** under `STATE-002`.
- **Owner:** 3.3 (STATE-002 v1.0 sqlite cutover)

## J-16 — Recover corrupt state / metadata

- **Operator action:** `caduceus recover-state …`
- **Reality:** `planned`. v0.1 ships no `recover-state` command;
  the v1.0 implementation is Task 3.4 (human review).
- **Owner:** 3.4 (STATE-003, human review)

## J-17 — Remove the cron job (without uninstall)

- **Operator action:** `hermes caduceus cron-remove`
- **Reality:** `working-production`. Removes all `name == "caduceus"`
  jobs from the registry and deletes the pulse wrapper.
- **Owner:** —

## J-18 — Uninstall (preserving user state)

- **Operator action:** `hermes caduceus cron-remove` then
  `hermes plugins remove caduceus`
- **Reality:** `working-production`. Removal leaves the user
  bridge, state directory, and config intact. Tested by
  `test_uninstall_preserves_user_state_and_config`.
- **Owner:** —

## J-19 — First-time dry run

- **Operator action:** label an issue, set
  `CADUCEUS_DRY_RUN=1` in the daemon env, observe
  `<state_dir>/runs/<run_id>.dry-run.md`
- **Reality:** `working-production` for the dry-run surface
  (`src/orchestration.rs`, `tests/dry_run_test.rs`). Cron tick
  still depends on J-08 / J-09.
- **Owner:** 2.1 (to actually invoke the dry run from the cron
  tick)

## J-20 — Review worker transcript after a run

- **Operator action:** read `<state_dir>/runs/<run_id>.transcript`
- **Reality:** `integrated-not-proven`. The supervisor thread
  forwards stderr to the transcript file (`src/main.rs:run_supervisor_mode`),
  but the bounded transcript writer (`RUN-003`) and the failure
  surfacing land in Task 2.5.
- **Owner:** 2.5 (RUN-003)

## Reproduction

```bash
# Adapter coverage for each journey
grep -nE "_cli_setup|_cli_doctor|_cli_status|_cli_cron_install|_cli_cron_remove|_handle_caduceus_status|_cron_install\b|_write_pulse_wrapper" __init__.py

# Cron job bridge
grep -nE "def cron_list_jobs|def cron_create_job|def cron_update_job|def cron_remove_job|def _dispatch" _runtime.py

# Tick path
grep -nE "fn run_blocking|fn exit_code_for|fn run_orchestration" src/tick.rs
grep -nE "fn run\b|fn run_worktree_gc|fn run_queue_reset|fn run_migrate_state" src/cli.rs

# Finalize path (J-10, J-12, J-13)
grep -nE "fn commit_and_push|fn find_or_create_pr|fn post_comment|fn transition_to_done" src/finalize.rs

# Migrate / recover
grep -nE "fn run\b|pub fn run\b" src/migrate.rs
grep -nE "recover_state|RecoverState" src/cli.rs src/recover.rs 2>/dev/null || echo "no recover.rs today (planned)"

# Uninstall
grep -nE "test_uninstall_preserves_user_state|test_source_update" tests/hermes_plugin_test.py
```