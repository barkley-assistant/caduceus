# State Recovery

The daemon owns its state. Operators own their recovery.
**Do not edit `state.db`, `state.json`, `state_meta.json`,
claim files, or transcripts in place.** SQLite's WAL
journal and the daemon's lock discipline only hold for
the programmatic API. This doc is the API.

The migration procedure from a prior installation is
in [`../MIGRATION.md`](../MIGRATION.md) at the
repository root. This doc covers in-place recovery,
which is different: state has become corrupt
in-place and the daemon is refusing to start.

## How to Detect Corruption

The daemon's loader runs `PRAGMA integrity_check` on
the SQLite store (`state.db`) at open time. When it
finds corruption, the daemon surfaces a
`StateCorrupt` error with the SQLite error message on
stderr:

```
cannot open SQLite store at /path/to/state.db:
database disk image is malformed
```

Other corruption signals:

- `caduceus status --json` reports `state_corrupt:
  true`.
- The daemon writes a corruption marker at
  `<state_dir>/state.db.corrupt`.
- The original corrupt file is archived at
  `<state_dir>/state.db.corrupt-<unix-ts>`.
- The daemon exits non-zero and refuses to call the
  GitHub API while the marker is present.

If you see a `StateCorrupt` error, use the recovery
path below.

## The Failure Modes

When the daemon detects corruption on the SQLite
store:

1. The file is preserved at its original path (no
   silent truncation; no overwrite with empty state).
2. A timestamped archive is written at
   `<state_dir>/state.db.corrupt-<unix-ts>`.
3. A corruption marker file is written at
   `<state_dir>/state.db.corrupt`.
4. The daemon exits with the `StateCorrupt` error and
   a non-zero exit code.

The daemon refuses to call the GitHub API while a
corruption marker is present. The documentation says
"use the recovery path"; this is what we mean.

## The Recovery Workflow

Recovery is a sequence, not a single command. Do not
skip steps.

### 1. Stop the Daemon

Whichever path you used to start it (Hermes cron,
system cron, manual invocation), kill the active tick.
The daemon's whole-tick flock handles in-flight locks,
but new ticks would race your recovery.

### 2. Inspect the Corruption

Read the marker file — its presence is the signal.
`cat $STATE_DIR/state.db.corrupt` returns immediately
(the file is empty; it's a flag). Then inspect the
archive at `state.db.corrupt-<unix-ts>` to understand
the scope of the damage:

```bash
# Check integrity on the archived copy.
sqlite3 $STATE_DIR/state.db.corrupt-<ts> 'PRAGMA integrity_check;'
```

The most common causes of SQLite corruption are:

- An unclean shutdown mid-write (WAL should prevent
  this; if you see it routinely, check your filesystem
  sync settings).
- Operator hand-edit (opening `state.db` with a text
  editor or hex editor breaks the B-tree).
- Filesystem corruption (rare; check `dmesg` for I/O
  errors).

### 3. Choose a Repair Strategy

You have three options, depending on what you have
available:

**Option A — Surgical repair (localised corruption)**: if
the corruption is in a specific row or table and you
can identify it, open the archived corrupt database
with `sqlite3` and delete or repair the affected rows:

```bash
sqlite3 $STATE_DIR/state.db.corrupt-<ts>
sqlite> PRAGMA integrity_check;
-- identifies the table / row
sqlite> DELETE FROM queue_entries WHERE issue_key = 'broken-repo#1';
sqlite> INSERT OR REPLACE INTO queue_entries (...) VALUES (...);
sqlite> PRAGMA integrity_check;  -- must return 'ok'
sqlite> .quit
cp $STATE_DIR/state.db.corrupt-<ts> $STATE_DIR/state.db
```

**Option B — Restore from a backup**: if you have a
known-good backup, validate it first:

```bash
sqlite3 /path/to/backup.db 'PRAGMA integrity_check;'
# Must return "ok".
```

Then install it. The daemon provides a library-level
recovery function (`caduceus::migrate::recover_sqlite_state`)
that archives the corrupt database, validates the
backup, and installs it atomically while holding the
daemon lock. If you are writing a recovery tool, use
that function. The canonical source is
`src/state/migrate.rs`.

**Option C — Start fresh**: if you have no backup and
the corruption is too broad to surgically repair,
remove the corrupt database and the marker and let
the daemon create a fresh store on the next tick:

```bash
# The daemon archives the corrupt file for you —
# it's already at state.db.corrupt-<ts>. You just
# need to remove the current (corrupt) database and
# the marker so the next start creates a fresh store.
rm $STATE_DIR/state.db
rm $STATE_DIR/state.db.corrupt
```

### 4. Verify the Repair

```bash
caduceus status --json
```

Should report `state_corrupt: false` and show the
recovered queue. If it does not, the repair did not
take — return to step 2.

### 5. Restart the Daemon

Restart through your usual path. The daemon will open
the store, run `PRAGMA integrity_check`, find it
clean, and proceed with the next tick.

## The `migrate-state` Subcommand

```text
caduceus migrate-state --from <file> [--dry-run]
```

Imports a JSON-formatted state file from a prior
installation into the current SQLite schema.
Documented in detail in `MIGRATION.md`. This is *not*
the same as recovery; recovery is for in-place
corruption, migration is for cross-format import.

If your `~/.caduceus/state.db` is already SQLite,
migration is not needed.

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

The daemon writes `state.db.corrupt-<unix-ts>` archives
during corruption recovery. The daemon does not
currently sweep these. Operators can `rm` old archives
manually:

```bash
# keep the most recent 5
ls -t $STATE_DIR/state.db.corrupt-* | tail -n +6 | xargs rm -f
```

A retention sweep inside the daemon is a future
feature; the operator is responsible for
housekeeping.

## Appendix: JSON Recovery (Pre-SQLite Migrations)

The procedure below applies to installations still
using the JSON backend (`state_backend: json`) or
operators recovering a `state.json` file from a
pre-SQLite install. If you have already migrated to
SQLite, use the SQLite recovery workflow above.

### JSON Failure Modes

The daemon's JSON loader validates `state.json` and
`state_meta.json` on every `state_dir` open. When it
finds a malformed file:

1. The file is preserved at its original path.
2. A timestamped archive is written at
   `<state_dir>/state.json.corrupt-<unix-ts>` (or
   `state_meta.json.corrupt-<unix-ts>`).
3. A corruption marker file is written at
   `<state_dir>/state.json.corrupt` (or
   `state_meta.corrupt`).
4. The daemon exits with `StateCorrupt` and a non-zero
   exit code.

### JSON Recovery Workflow

1. **Stop the daemon.**
2. **Read the marker file.** The marker file is empty;
   its presence is the signal.
3. **Read the archive at `state.json.corrupt-<ts>`.**
   Common causes: half-written file from a crash
   mid-write; operator hand-edit; filesystem
   corruption.
4. **Build a repaired file.** The repaired file must
   be a valid `QueueState` envelope. The easiest path
   is `caduceus migrate-state --from <file>`.
5. **Apply the repaired file** via
   `caduceus::migrate::recover_state()` (library API,
   recommended) or the temp+fsync+rename pattern
   (direct install, bypasses lock protection).
6. **Verify** with `caduceus status --json`.
7. **Restart the daemon.**

## When to File a Bug

- The daemon wrote a `state.db.corrupt-<ts>` archive
  whose content passes `PRAGMA integrity_check` (this
  means the daemon's loader had a false positive;
  please file with the archive attached).
- The daemon refused a recovery that, in your
  judgement, was valid (attach both the corrupted
  original and your repaired file).
- The recovery succeeded but the daemon's behaviour on
  the next tick was wrong (attach the recovered state
  and the relevant tick log).

In all cases, file at the project's GitHub issues. Do
not include secrets.
