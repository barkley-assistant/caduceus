# Caduceus migration and recovery

This document is the operator-facing runbook for migrating legacy
Caduceus state into the v0.1 schema and for recovering from a corrupt
state directory. It is the canonical reference for
`caduceus migrate-state`, the corruption-marker recovery flow, the
manual `caduceus queue reset` path, and the rollout / rollback
procedure.

> **Do not edit `state.json`, `state_meta.json`, or claim files in
> place.** The daemon owns these files. Use the supported commands
> documented below; the daemon takes the lock, validates input, and
> installs the result atomically.

## 0. Before you start

- Run Caduceus against one disposable repository first. Two active
  processors (the legacy one and Caduceus) targeting the same labels
  will both act on the same issue; that is forbidden during
  cutover. See `planning/caduceus-v0.1/tasks/9.2-execute-the-release-gate-and-cutover-checklist.md`
  for the cutover checklist.
- Disable the legacy processor's cron entry before you install the
  Caduceus state import. The daemon's lock is process-local, so the
  legacy processor will not be blocked by Caduceus's daemon.lock and
  will resume polling as soon as its cron fires.
- Read your current `CADUCEUS_CONFIG` (or the resolved
  `state_dir`) and confirm the path. Migration writes into that
  directory under `<state_dir>/state.json` and never reads or writes
  anything outside it.

## 1. Import legacy state

```text
caduceus migrate-state --from <legacy.json> [--dry-run]
```

The `--from` path is the legacy v0 state file (a JSON envelope with
an `entries` array of `{repo, number, status, ...}` records). The
command:

- takes the daemon lock (a concurrent tick wins the race; rerun
  later);
- parses the legacy envelope and validates each entry against the
  current `IssueKey` rules;
- imports any entries that are not already in the live state;
- reports duplicates as `entries_skipped` (not errors);
- leaves the live state untouched when the input is malformed;
- installs the new state with the canonical atomic-write pattern
  (temp file + fsync + rename);
- preserves the prior content as `<state_dir>/state.json.bak-<ts>`;
  a copy of the just-installed content is also written alongside
  when there was no prior content, so the operator always has a
  rollback target.

### Dry-run rollout

Always run with `--dry-run` first:

```text
$ caduceus migrate-state --from /tmp/legacy.json --dry-run
caduceus migrate-state: dry-run; would import N, would skip M
```

Dry-run reads everything (including the live state under the
lockless `snapshot` path) but never installs. Repeat the dry-run
until the `would import` count matches your expectation, then run
without `--dry-run`.

### Idempotency

A second `caduceus migrate-state` against the same input is a
no-op and prints `already current; no changes`. The command never
duplicates entries. Entries with the same `owner/repo#number` key
that already exist in the live state are reported as `skipped` and
left untouched; if the input and live state disagree on the entry
content, the migration refuses to overwrite and reports the
conflict as skipped. Use `caduceus queue reset` for an explicit
operator-driven reset (see §4 below).

### Rollback

The prior content is preserved at
`<state_dir>/state.json.bak-<unix-ts>`. To roll back:

```text
$ # Stop the daemon (cron / supervisor).
$ ls "$STATE_DIR"/state.json.bak-* | tail -n 1
$ cp "$STATE_DIR"/state.json.bak-<latest> "$STATE_DIR"/state.json
$ # Restart the daemon.
```

A `state.json.corrupt-<unix-ts>` archive is emitted by the
recovery path (§3) when the daemon had previously detected a
corrupt file. Operators may inspect these to confirm what was
rejected.

## 2. Credential-helper setup

Caduceus does **not** inject GitHub credentials into the worker or
git environment. The daemon only holds the token at the
configuration-resolution step; the worker never sees it. Configure
your environment so the daemon, the cron invoker, and `git push`
all reach GitHub through the same credential-helper or SSH agent
the operator uses:

- For HTTPS repos, set the system credential helper
  (`git config --global credential.helper <helper>`) and either
  `CADUCEUS_GITHUB_TOKEN` / `GITHUB_TOKEN` / `gh auth token` so
  `git push` and the daemon use the same token.
- For SSH repos, ensure the cron invoker's `~/.ssh/config` and
  `SSH_AUTH_SOCK` are reachable from the daemon's process.

The daemon never logs token values. `caduceus status` reports the
last HTTP status; a `401` or `403` after a previously-green tick
indicates the credential expired.

## 3. Corrupt-state recovery

When the daemon's loader finds a malformed `state.json` it
preserves the original bytes as `<state_dir>/state.json.corrupt-<ts>`
and refuses to start until recovery completes. The `state.json.corrupt`
marker is informational; the preserved file is the recovery target.
**Never edit the corrupt file in place.**

Recovery validates a supplied repaired file under the daemon lock,
atomically installs it, archives the corrupt original, and only
then clears the corruption marker. Use the API directly when
scripting:

```rust
use caduceus::migrate::recover_state;

let report = recover_state(&repaired_path, &state_dir, /*clear_marker=*/ true, /*hold_daemon_lock=*/ true)?;
println!("archived at: {:?}", report.archived_corrupt);
```

Or via the migration test surface for scripted recovery: build a
valid v1 `QueueState`, serialize it to a file, then call
`recover_state`. The same shape is produced by `migrate-state`'s
output, so an operator can always derive a recovered file by
re-running the migration on a clean legacy input.

A malformed repaired file is rejected; the original corrupt file
remains archived, the active state file is unchanged, and the
marker is not cleared.

## 4. Failed-entry inspection and reset

A `Failed` queue entry that you want to retry:

```text
$ caduceus status                                     # find the issue
$ caduceus queue reset owner/repo#number --dry-run    # inspect the planned change
$ caduceus queue reset owner/repo#number              # apply
```

The reset is non-destructive by default: the entry moves back to
`Queued` and the persisted `FinalizationCheckpoint` (branch / PR
/ run ID / commit OID) is preserved so a follow-up tick resumes
from the saved state. Use `--force-finalization-reset` to drop
the checkpoint; the daemon then prints a warning listing the
branch and PR URL and leaves the remote branch and PR alone for
manual reconciliation. The daemon never deletes remote branches
or PRs.

Removing and re-adding the trigger label is **not** a substitute
for `caduceus queue reset`; the budget of three total worker
attempts is preserved across label churn and only the explicit
reset path clears it.

## 5. Caduceus uninstall and state preservation

Caduceus has no plugin uninstall hook in Hermes. Operators run the
following sequence in order; each step is idempotent:

```text
$ hermes caduceus cron-remove           # removes the no-agent cron job + wrapper
$ hermes plugins remove caduceus        # tears down the plugin
```

The state directory (`$HERMES_HOME/caduceus/`-equivalent), the
daemon state directory (`<state_dir>`), the user-owned bridge
(`$HERMES_HOME/caduceus/worker-bridge.py`), and the operator's
configuration (`~/.config/caduceus/config.yaml`) are all
**preserved**. Reinstalling the plugin against the same state
directory resumes the daemon where the last tick finished.

Caduceus also preserves the watched repositories and any worktrees
they contain. `caduceus worktree-gc` cleans up after the daemon is
removed.

## 6. Cutover checklist summary

1. Stop the legacy processor's cron entry.
2. Run `caduceus migrate-state --from <legacy.json> --dry-run`.
3. Compare `would import` against the legacy file's `entries` count.
4. Run `caduceus migrate-state --from <legacy.json>` (no `--dry-run`).
5. Verify with `caduceus status` and inspect
   `<state_dir>/state.json.bak-*`.
6. Enable the Caduceus cron entry
   (`hermes caduceus cron-install`).
7. Watch one tick on a disposable repository; verify status, retry
   rate-limit handling, and (when needed) rollback via the backup.

If anything looks wrong, restore the backup file, disable the
Caduceus cron entry, and re-enable the legacy cron entry to roll
back.

## 7. Related references

- `planning/caduceus-v0.1/CONTRACTS.md` — the normative schema and
  CLI contract.
- `planning/caduceus-v0.1/phases/09-release.md` — the phase 9 gate
  this document ships against.
- `src/migrate.rs` — the implementation; `tests/migration_test.rs`
  pins the migration and recovery tests.
