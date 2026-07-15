# Caduceus Skill

Resolves as `caduceus:caduceus` after the plugin is loaded on Hermes
Agent v0.18.2 (the minimum tested host). Plugin skills are opt-in: the
skill does not appear in the system prompt's `<available_skills>`
block and conversational phrases do not trigger it automatically. Load
it explicitly through `skill_view("caduceus:caduceus")` when the user
asks about the daemon.

## Intended uses

Load this skill when the user asks things like:

- "What issues is caduceus working on?"
- "Show me the caduceus queue"
- "Why isn't caduceus picking up issue #338?"
- "How do I configure caduceus for repo X?"
- "Set up auto-fix for org Y"
- "Swap caduceus to use pi instead of opencode"
- "Why is caduceus silently skipping my repo?"
- "How do I recover from a corrupt queue file?"

## Workflow

When triggered, this skill should:

1. **Check daemon status** by running `/caduceus-status` from chat (or
   `caduceus status` from a shell). Parse the JSON or human output.
2. **If the user wants to know what's happening**: surface the queue
   contents, last-run timestamps, retry counts, recent errors.
3. **If the user wants to configure**: walk them through the
   `caduceus:` section of `~/.hermes/config.yaml`, then verify watched
   repositories exist at `<workdir_base>/<owner>/<repo>` with a matching
   noninteractive `origin`. Reference the plugin's defaults.
4. **If the user wants to swap harnesses**: explain that they edit the
   plugin's user-owned `worker-bridge.py` (default
   `~/.hermes/caduceus/worker-bridge.py`) and change one function
   (`invoke_harness`). Leave `read_required_env` / `parse_labels` /
   `verify_prompt` alone — those enforce the daemon contract.
5. **If something is broken**: tail `<state_dir>/processor.log` and
   `<state_dir>/runs/<run-id>.log` for the affected run. For a terminal
   failed/skipped entry, show `caduceus queue reset OWNER/REPO#N
   --dry-run` before proposing the real reset; never edit state files
   directly.
6. **If the user asks about a stuck `Failed` issue**: explain that
   failed entries are not auto-reset. Removing and re-adding the
   trigger label does not bypass the per-issue retry budget; the
   operator must run `caduceus queue reset OWNER/REPO#N [--dry-run]`.
7. **If the user asks about dry-run behavior**: explain that
   `CADUCEUS_DRY_RUN=1` performs polling, claim, prompt creation, worker
   execution, and result validation but performs **no** commit, push,
   comment, label mutation, PR, or issue close. Successful dry-runs
   transition to `Previewed`; disabling dry-run promotes a still-labeled
   preview back to `Queued` automatically.
8. **If the user asks about transcript locality**: each run writes
   `<state_dir>/runs/<run_id>.log` (the worker transcript) and
   `<state_dir>/runs/<run_id>.dry-run.md` (only when dry-run). The
   daemon `processor.log` lives at `<state_dir>/processor.log` and the
   heartbeat envelope sits at `<state_dir>/runs/<run_id>.heartbeat`.

## Investigation vs. Code Tickets

The same bridge contract serves both. The bridge forwards labels via
`CADUCEUS_ISSUE_LABELS_JSON` and the harness decides how to branch.

- **Code ticket** (`🤖 auto-fix`): worker success → commit + push + open
  PR + post completion comment + close issue.
- **Investigation ticket** (`🤖 auto-fix-investigate`): worker success
  → post findings comment + remove trigger label + leave issue open.
  No commit, no push, no PR, no close.

Both tickets use the same `worker-result.json` schema; for
investigation the `commit_message` and `pull_request_title` fields are
still required but ignored. The bridge never forks behavior — the
harness does.

## Retry Budget

The per-issue retry counter (`max_retries_per_issue`, default 3) only
increments on **worker-attributable** failures (the harness exited
non-zero). Worker failure 1 or 2 → back to `Queued` with
`next_attempt_at = now + retry_backoff_seconds`. Worker failure 3 →
`Failed` and the issue stops being claimed.

GitHub / git transport / local I/O / rate-limit / operator-cancellation
failures do **not** consume the worker budget. They count as transient
and the daemon retries on the next tick without bumping the per-issue
counter.

## State Recovery Procedure

Both `queue.json` and `state_meta.json` use temp-file + `fsync` +
atomic rename and are never silently truncated:

- **Corrupt `queue.json`** → daemon exits 1, file preserved. Inspect
  and repair manually or use `caduceus migrate-state --from <path>
  [--dry-run]`.
- **Corrupt `state_meta.json`** → same behavior. Exit 1, file preserved.
- **Stale heartbeat** (>90s old) → reaped on the next tick after
  `stale_run_hours` elapses.
- **Stuck issue** → `caduceus queue reset OWNER/REPO#N [--dry-run]`.
  The reset requires the daemon's whole-tick lock and refuses to drop
  an entry with an open PR unless `--force-finalization-reset` is
  supplied and confirmed in dry-run output.
- **Never edit state files directly.** Manual intervention is not a
  supported path; the daemon's lock + atomic-write discipline only
  holds for the programmatic API.

## Boundaries

- **Do not edit daemon state files** directly. Use the documented
  migration/recovery commands; malformed state is preserved for
  diagnosis.
- Multiple cron invocations are safe: a host-wide nonblocking lock allows
  one tick and makes later invocations exit cleanly. Do not bypass that
  lock with custom tooling.
- **Do not edit user-modifiable config** without confirming with the user
  first.
- The plugin's bridge template (`plugin-assets/worker-bridge.py`) is a
  starting point, not a constraint. Switching harnesses means editing the
  *user-owned* copy under `$HERMES_HOME/caduceus/`; the adapter never
  overwrites it on `setup`. If the upstream template changes, setup
  writes a sibling `.new` candidate and reports it; your edits remain
  intact.
- **The plugin does not run manifest build/hook steps.** Hermes
  installs plugin source but does not auto-build or auto-execute; you
  must run the explicit `hermes caduceus setup` step yourself.

## Setup

The plugin does not run manifest build/hook steps. After
installing or updating the plugin, the operator runs:

```bash
hermes plugins install barkley-assistant/caduceus --enable
hermes caduceus setup
hermes caduceus cron-install
```

`setup` builds the Rust binary with `cargo build --release --locked`,
installs it atomically at `<plugin>/bin/caduceus`, creates the state
directories with mode 0700, and seeds the user-owned bridge under
`$HERMES_HOME/caduceus/`. `cron-install` creates the
`caduceus-pulse.sh` wrapper and reconciles a single no-agent 2-minute
Hermes cron job that calls it. The Hermes gateway (or a configured
managed cron provider) must be running for the cron job to fire.

### Source updates

When a new plugin version is released:

```bash
hermes plugins update caduceus
hermes caduceus setup          # rebuild + atomic binary replacement
hermes caduceus cron-install   # idempotent: 0/1/N matches handled
```

The `hermes plugins update` step updates sources only — it does **not**
rebuild the binary. Always re-run `hermes caduceus setup` afterwards
to pick up the new Rust workspace. `setup` preserves the user-owned
bridge and only writes a sibling `.new` candidate if the upstream
template changes.

### Standalone installs (no Hermes)

If the operator installed Caduceus without going through Hermes, the
daemon's `worker_command` config must be set explicitly — the default
that points at the seeded user-owned bridge only applies after a
Hermes `setup` step. The daemon refuses to start with a placeholder
`worker_command` and surfaces a precise missing-worker instruction in
the log.

## Removal

Hermes has no plugin-uninstall hook. Operators run:

```bash
hermes caduceus cron-remove
hermes plugins remove caduceus
```

`cron-remove` removes the cron job and the wrapper idempotently.
Removal preserves `$HERMES_HOME/caduceus/`, the daemon state directory,
user config, and repositories — none of those are touched by
`plugins remove`.