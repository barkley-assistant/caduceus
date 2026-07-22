# State Recovery

The daemon owns its state. Operators own their recovery.
**Do not edit `state.json`, `state_meta.json`, claim
files, or transcripts in place.** The lock and
atomic-write discipline only hold for the programmatic
API. This doc is the API.

The migration procedure from a prior installation is
in [`../MIGRATION.md`](../MIGRATION.md) at the
repository root. This doc covers in-place recovery,
which is different: state has become corrupt
in-place and the daemon is refusing to start.

## The Failure Modes

The daemon's loader validates `state.json` and
`state_meta.json` on every `state_dir` open. When it
finds a malformed file:

1. The file is preserved at its original path (no silent
   truncation; no overwrite with empty state).
2. A timestamped archive is written at
   `<state_dir>/state.json.corrupt-<unix-ts>` (or
   `state_meta.json.corrupt-<unix-ts>`).
3. A corruption marker file is written at
   `<state_dir>/state.json.corrupt` (or
   `state_meta.corrupt`).
4. The daemon exits with the `StateCorrupt` error and
   a non-zero exit code.

The daemon refuses to call the GitHub API while a
corruption marker is present. The documentation says
"use the recovery path"; this is what we mean.

## The Recovery Workflow

Recovery is a sequence, not a single command. Do not
skip steps.

1. **Stop the daemon.** Whichever path you used to
   start it (Hermes cron, system cron, manual
   invocation), kill the active tick. The daemon's
   whole-tick flock handles in-flight locks, but new
   ticks would race your recovery.
2. **Read the marker file.** The marker file is empty;
   its presence is the signal. `cat
   $STATE_DIR/state.json.corrupt` (or
   `state_meta.corrupt`) will return immediately.
3. **Read the archive.** The original file lives at
   `state.json.corrupt-<ts>` (or the metadata
   equivalent). Open it; understand what's wrong. The
   most common causes are:
   - A half-written file from a crash mid-write (the
     atomic-write discipline should prevent this; if
     you see it, file a bug).
   - Operator hand-edit (the daemon never does this;
     see the warning at the top of this doc).
   - Filesystem corruption (rare; check `dmesg` for
     I/O errors).
4. **Build a repaired file.** The repaired file must
   be a valid envelope:
   - `state.json` must parse as the `QueueState`
     schema (`entries` as a map of display keys to
     `QueueEntry` records).
   - `state_meta.json` must parse as the `StateMeta`
     schema.
   If you don't know how to write that by hand, the
    easiest path is to run `caduceus migrate-state
    --from <file>` against a prior-state JSON file
    (see `MIGRATION.md`).
5. **Apply the repaired file.** There are two paths
   here; pick the one that fits the situation:
   - **Library API:** if you have a Rust binary at
     hand and want the safe, daemon-lock-protected
     path, call `caduceus::migrate::recover_state(
     repaired_path, state_dir, /*clear_marker=*/ true,
     /*hold_daemon_lock=*/ true)`. The function
     archives the corrupt original, atomically
     installs the repaired file, and only then clears
     the corruption marker. The canonical source for
     this API is `src/state/migrate.rs` in the Caduceus
     source tree.
   - **Direct install:** if you understand what
     you're doing, manually move the corrupt file
     aside (rename `state.json` →
     `state.json.corrupt-<your-ts>`), write the
     repaired content with the canonical
     temp+fsync+rename pattern, and remove the
     corruption marker with `rm
     <state_dir>/state.json.corrupt`. **This path
     bypasses the daemon-lock protection and the
     library's archive logic.** Use the library path
     if you can.
6. **Verify.** `caduceus status --json` should report
   the recovered state and a clean `state_corrupt:
   false`. If it doesn't, do not push; the recovery
   didn't take.
7. **Restart the daemon.**

## The `migrate-state` Subcommand

```text
caduceus migrate-state --from <file> [--dry-run]
```

Imports a JSON-formatted state file from a prior installation into
the current schema. Documented in detail in `MIGRATION.md`. This is
*not* the same as recovery; recovery is for in-place corruption,
migration is for cross-format import.

If your `~/.caduceus/caduceus.db` is already SQLite, migration is
not needed.

## The `queue reset` Subcommand

```text
caduceus queue reset owner/repo#number [--dry-run] [--force-finalization-reset]
```

The recovery operation for a `Failed` or `Skipped`
entry. Moves the entry back to `Queued`. The persisted
`FinalizationCheckpoint` (branch / PR / run ID / commit
OID) is preserved by default so a follow-up tick
resumes from the saved state. `--force-finalization-reset`
drops the checkpoint and the daemon prints a warning
listing the branch and PR URL; the daemon never
deletes remote branches or PRs.

The subcommand takes the daemon lock and refuses
entries with an active claim file. **Removing and
re-adding the trigger label is not a substitute** for
this command; the budget of three total worker
attempts is preserved across label churn and only the
explicit reset path clears it.

## Backup Retention

Every migration install writes a new
`state.json.bak-<unix-ts>` to the state directory.
The daemon does not currently sweep these. Operators
can `rm` old backups manually:

```bash
# keep the most recent 5
ls -t $STATE_DIR/state.json.bak-* | tail -n +6 | xargs rm -f
```

A retention sweep inside the daemon is a future
feature; the operator is responsible for
housekeeping.

## When to File a Bug

- The daemon wrote a `state.json.corrupt-<ts>` archive
  whose content is parseable as the current schema
  (this means the daemon's loader had a false positive;
  please file with the archive attached).
- The daemon refused a recovery that, in your
  judgement, was valid (attach both the corrupted
  original and your repaired file).
- The recovery succeeded but the daemon's behaviour on
  the next tick was wrong (attach the recovered state
  and the relevant tick log).

In all cases, file at the project's GitHub issues. Do
not include secrets.