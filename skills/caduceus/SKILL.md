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
4. **If the user wants to swap harnesses**: explain the new
   one-function contract. They copy
   `plugin-assets/caduceus_harness.py.example` to
   `~/.hermes/caduceus/harness.py` (alongside any existing
   `worker-bridge.py`) and edit `run_task(ctx)` to return the
   harness argv. The parent runner in `plugin-assets/worker-bridge.py`
   loads `harness.py` when present, calls `run_task(ctx)`, runs the
   returned argv inside the worktree, and synthesizes
   `worker-result.json` if the hook didn't write one. Existing
   311-line `worker-bridge.py` copies keep working unchanged on the
   legacy path. To verify the active path, run
   `ls ~/.hermes/caduceus/`: a `harness.py` file means the new
   contract; only `worker-bridge.py` means the legacy path. The parent
   resolves this relative to `$HERMES_HOME` (default `~/.hermes`). The
   bridge is target-neutral: a PR review run exports
   `CADUCEUS_WORK_TARGET=pr` plus the `CADUCEUS_PR_*` vars, resolves
   only the mode-correct result path, and never synthesizes a result
   (no result file = execution failure). Re-seed the user-owned bridge
   (`hermes caduceus setup`) before enabling review runs — an
   un-updated copy fails closed on a PR env while issue-path runs keep
   working unchanged.
5. **If something is broken**: tail `<state_dir>/processor.log` and
   `<state_dir>/runs/<run-id>.log` for the affected run. For a terminal
   failed/skipped entry, show `caduceus queue show OWNER/REPO#N` to
   inspect the entry and checkpoint, then `caduceus queue reset
   OWNER/REPO#N [--dry-run]` before proposing the real reset; never edit
   state files directly. If the root cause is fixed and the operator
   wants a fresh run, `caduceus queue reprocess
   OWNER/REPO#N [--dry-run]` creates a new generation and makes the
   entry immediately claimable.
6. **If the user asks about a stuck `Failed` issue**: explain that
   failed entries are not auto-reset. Removing and re-adding the
   trigger label does not bypass the per-issue retry budget; the
   operator must run `caduceus queue reset OWNER/REPO#N [--dry-run]`.
   If instead a brand-new generation is wanted (for example the fix
   landed elsewhere and the issue should be re-run), `caduceus
   queue reprocess OWNER/REPO#N [--dry-run]` increments the
   generation counter and moves the entry back to `Queued` with no
   backoff, immediately claimable; it refuses only `AwaitingReview`.
   If the operator wants to drop the entry entirely instead of
   retrying, `caduceus queue remove OWNER/REPO#N [--dry-run]` deletes
   only the queue entry (never the remote branch/PR, worktree, or
   claim file).
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

## Configuration keys

Set `git_author_name` and `git_author_email` in the `caduceus:` block to
configure commit identity. Each field cascades independently from explicit
config to the host's global git config and then
`Caduceus Daemon <caduceus@daemon.local>`, so values from different tiers can
merge; the daemon emits a once-per-process WARN when the last-resort fallback
is used.

## Code Tickets

The bridge contract serves code tickets. The bridge forwards labels via
`CADUCEUS_ISSUE_LABELS_JSON` and the harness decides how to branch.

- **Code ticket** (`autofix`): worker success → commit + push + open
  PR + post completion comment + close issue.

The `worker-result.json` schema is fixed; the bridge never forks
behavior — the harness does. (Investigation tickets were removed in
release N+1; see docs/release-notes.md.)

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

## Per-Tick Claim Cap

`max_issues_per_tick` (default `worker_parallelism * 4`) bounds how
many queue entries a single tick will claim before returning. With the
 default, a tick with `worker_parallelism: 4` processes up to 16 issues
and leaves the rest for the next tick. Set `0` to restore the unbounded
drain-the-queue behavior. In-flight workers always finish their current
work on tick exit; the cap only stops claiming new entries.

## Auto Review

The `auto_review:` block (default absent = disabled) opts the daemon
into automatic review of every eligible PR revision in watched repos.
`auto_review.enabled: true` requires `executor_mode: oci` with a valid
`sandbox:` section — TrustedHost + enabled fails at config load (reviews
execute third-party tooling against untrusted PR content). The
`autoreview` GitHub label is reserved but inert in Phase 1: no daemon
code polls it and PR eligibility never requires it. Draft PRs are
skipped unless `auto_review.draft_pull_requests: true`.
`max_reviews_per_tick` (default `worker_parallelism * 4`, `0` =
unbounded) bounds per-tick review admission. Investigation tickets
were removed in release N+1 (#331): the `ticket_label_investigation`
config key now fails the config load, and surviving investigation
rows are terminated and archived by the startup reconcile pass.

### Fork PR review (issue #337, Phase 2)

Fork PRs are skipped by default (`review_skipped_fork_unsupported`).
To review forks of a watched repo, list the repo slug under
`auto_review.fork_policy.allow_fork_prs`:

```yaml
auto_review:
  enabled: true
  fork_policy:
    allow_fork_prs:
      - owner/repo
```

- The list is a per-repo **opt-in** (default empty → fail-closed);
  slugs must be watched repos (config load rejects unknown slugs).
- Allowed forks are reviewed through a **per-PR quarantine clone**
  under `<state_dir>/fork-quarantine/`, never a second remote on the
  production mirror; the clone is removed at terminal status or by
  the per-tick orphan sweep.
- **Private forks**: listing a repo means the daemon's PAT read scope
  is acceptable for that fork's visibility. See
  `docs/security/fork-trust-posture.md`.
- Denied forks keep the Phase-1 skip event byte-for-byte.

Review observability (issue #318, DAR §13):

- `caduceus review status [OWNER/REPO] [--json]` — aggregate review
  queue phase counts plus one line per entry; an optional repository
  filter narrows the report. `--json` emits the `review/1.0` envelope
  with `counts` and the full per-row field set (repo, PR, base SHA,
  head SHA, merge base, review state, run id, review generation,
  execution attempts, execution status, verdict, last error,
  reviewed-at, publication state, publication attempt count, next
  publication attempt).
- `caduceus review list [--json]` — every review queue entry as a
  table (full per-row fields in JSON).
- `caduceus review show OWNER/REPO PR [--json]` — full detail plus
  run history for one PR; history `result_json` documents are parsed
  defensively (older schema versions surface raw with a
  `parse_error`, never back-migrated). A missing entry yields a
  `"no_entry"` diagnostic on the JSON path.

All three read BOTH state backends (they branch on
`state_backend == "sqlite"` like the daemon, not like the JSON-only
`queue` CLI), never take the daemon lock, and never write state.
`execution status` is a derived presentation field parsed from the
latest same-generation history row's `ReviewResult.status` — it
describes execution, not outcome (`verdict` holds the outcome; a
failed-verdict run is still `execution status: success`).
`verdict` is per-entry too: parsed from the SAME latest
same-generation history row's `ReviewResult.review.verdict`, so
superseded SHA entries show their own outcome; only entries with no
completed run fall back to the PR-level last verdict (#387).
Unparsable result documents surface `-`/null, never the PR-level
verdict.

## State Recovery Procedure

Both `state.json` and `state_meta.json` use temp-file + `fsync` + atomic
rename and are never silently truncated:

- **Corrupt `state.json`** → daemon exits 1, file preserved. Inspect
  and repair manually or use `caduceus migrate-state --from <path>
  [--dry-run]`.
- **Corrupt `state_meta.json`** → same behavior. Exit 1, file preserved.
- **Stale heartbeat** (>90s old) → reaped on the next tick after
  `stale_run_hours` elapses.
- **Stuck issue** → inspect first with `caduceus queue show` (list
  form) or `caduceus queue show OWNER/REPO#N` (full detail including
  the finalization checkpoint), then recover with
  `caduceus queue reset OWNER/REPO#N [--dry-run]`. The reset requires
  the daemon's whole-tick lock and refuses to drop an entry with an
  open PR unless `--force-finalization-reset` is supplied and
  confirmed in dry-run output. To start a fresh generation instead
  (reopen, or fast-track a retry once the root cause is fixed),
  `caduceus queue reprocess OWNER/REPO#N [--dry-run]` increments
  the generation counter and moves a terminal entry back to
  `Queued`, immediately claimable on the next tick; it refuses
  only `AwaitingReview`.
- **Drop an entry entirely** → `caduceus queue remove OWNER/REPO#N
  [--dry-run] [--force]`. Remove deletes only the queue entry; the
  worktree, claim file, remote branch, and PR are left for the reaper
  / `worktree-gc` and are never touched. `InProgress`,
  `AwaitingReview`, and `Done` are refused by default; `--force`
  relaxes the phase guard only — an entry with a live claim file is
  always refused. If the trigger label is still on the issue, the
  next poll re-enqueues a fresh entry.
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
- The plugin (`plugin.yaml`, `__init__.py`, and this skill) does not know
  about harness-specific shapes. To roll back the new contract, remove
  `~/.hermes/caduceus/harness.py`; the parent runner then uses the legacy
  `worker-bridge.py` path.
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
