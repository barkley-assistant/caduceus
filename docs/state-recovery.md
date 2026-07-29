# State Recovery

The daemon owns its state. Operators own their recovery.
The daemon uses SQLite (`<state_dir>/state.db`) as its
primary state store. Legacy installations may still have
JSON files (`state.json`, `state_meta.json`) from the
v0.1.x era; see the appendix for the JSON recovery path.

**Do not edit `state.db`, claim files, or transcripts in
place.** The lock and transaction discipline only hold for
the programmatic API. This doc is the API.

The migration procedure from a prior installation is in
[`../MIGRATION.md`](../MIGRATION.md) at the repository
root. This doc covers in-place recovery, which is
different: state has become corrupt in-place and the
daemon is refusing to start.

## Detecting Corruption (SQLite)

The daemon's SQLite store validates the database on every
`open_in()`. When it detects corruption, it surfaces a
`StateCorrupt` error with a descriptive message and exits
non-zero. Unlike the JSON backend, the SQLite store does
**not** write marker files — corruption is surfaced as an
open/query error.

### Common error messages

| Error pattern | Likely cause |
|---|---|
| `cannot open SQLite store at ...` | `state.db` is missing, permissions are wrong, or the file is not a valid SQLite database |
| `cannot set pragmas: ...` | WAL journal mode could not be enabled (filesystem or permission issue) |
| `SQLite store has schema v{N} but this daemon only supports v{M}` | The database was written by a newer daemon version; upgrade the daemon |
| `cannot read schema_version: ...` | The schema_version table is missing or corrupted |
| `cannot begin IMMEDIATE transaction: ...` | The database file is locked by another process or corrupted |
| `PRAGMA integrity_check` output shows errors | Run `PRAGMA integrity_check` manually (see recovery workflow below) |

### Manual integrity check

```bash
sqlite3 $STATE_DIR/state.db "PRAGMA integrity_check;"
```

A healthy database prints `ok`. Any other output indicates
corruption; proceed to the recovery workflow.

## The Recovery Workflow (SQLite)

Recovery is a sequence, not a single command. Do not skip
steps.

1. **Stop the daemon.** Whichever path you used to start
   it (Hermes cron, system cron, manual invocation), kill
   the active tick. The daemon's whole-tick `daemon.lock`
   flock handles in-flight locks, but new ticks would race
   your recovery.

2. **Run integrity check.** Capture the current state of
   corruption:

   ```bash
   sqlite3 $STATE_DIR/state.db "PRAGMA integrity_check;"
   ```

   If this returns something other than `ok`, save the
   output for diagnostics.

3. **Create a safe backup.** Use one of these methods:

   ```bash
   # Method A: .backup (preferred — live-safe, consistent snapshot)
   sqlite3 $STATE_DIR/state.db ".backup $STATE_DIR/state.db.bak-$(date +%s)"

   # Method B: VACUUM INTO (requires SQLite 3.27+; rewrites the file)
   sqlite3 $STATE_DIR/state.db \
     "VACUUM INTO '$STATE_DIR/state.db.vacuum-$(date +%s)';"
   ```

   The backup is your rollback point. Keep it until the
   daemon has processed at least one successful tick after
   recovery.

4. **Attempt surgical repair.** If the corruption is
   localised to specific rows, you can repair in-place
   with SQL:

   ```bash
   # Remove entries with corrupt data (identified by error messages)
   sqlite3 $STATE_DIR/state.db \
     "DELETE FROM queue_entries WHERE issue_key = 'owner/repo#123';"

   # Re-insert a corrected entry
   sqlite3 $STATE_DIR/state.db \
     "INSERT OR REPLACE INTO queue_entries \
      (issue_key, phase, ticket_type, attempts, queued_at, updated_at) \
      VALUES ('owner/repo#123', 'queued', 'code', 0, \
              '$(date -u +%Y-%m-%dT%H:%M:%SZ)', \
              '$(date -u +%Y-%m-%dT%H:%M:%SZ)');"
   ```

   Use `caduceus status --json` to inspect the current
   queue before repairing. For widespread corruption, use
   the full-replace path (step 5a).

5. **Replace the database (full recovery).** When surgical
   repair is not feasible:

   a. **Dump salvageable data** (if the database is
      partially readable):

      ```bash
      sqlite3 $STATE_DIR/state.db \
        ".output $STATE_DIR/state.db.dump.sql" ".dump"
      ```

   b. **Move the corrupt database aside:**

      ```bash
      mv $STATE_DIR/state.db $STATE_DIR/state.db.corrupt-$(date +%s)
      mv $STATE_DIR/state.db-wal $STATE_DIR/state.db-wal.corrupt-$(date +%s) 2>/dev/null; true
      mv $STATE_DIR/state.db-shm $STATE_DIR/state.db-shm.corrupt-$(date +%s) 2>/dev/null; true
      ```

   c. **Create a fresh database** (the daemon initialises
      one automatically on next start, but you can also
      use `caduceus migrate-state --to-sqlite` from a JSON
      backup):

      ```bash
      # If you have a JSON backup:
      caduceus migrate-state --to-sqlite

      # If you have a SQL dump from step (a):
      sqlite3 $STATE_DIR/state.db < $STATE_DIR/state.db.dump.sql
      ```

   d. **Verify the fresh database** starts cleanly:

      ```bash
      sqlite3 $STATE_DIR/state.db "PRAGMA integrity_check;"
      ```

6. **Verify with the daemon.** Run the status command to
   confirm the daemon can load the recovered state:

   ```bash
   caduceus status --json
   ```

   Expect a normal status response. If it fails with a
   `StateCorrupt` error, the recovery did not take —
   restore from the backup created in step 3 and retry.

7. **Restart the daemon.**

## Appendix: JSON Recovery Path (Legacy)

This section applies only to pre-migration installations
still using `state.json` and `state_meta.json` (the `"json"`
state backend). If you have already migrated to SQLite,
skip this appendix.

### Failure modes (JSON)

The daemon's loader validates `state.json` and
`state_meta.json` on every `state_dir` open. When it finds
a malformed file:

1. The file is preserved at its original path (no silent
   truncation; no overwrite with empty state).
2. A timestamped archive is written at
   `<state_dir>/state.json.corrupt-<unix-ts>` (or
   `state_meta.json.corrupt-<unix-ts>`).
3. A corruption marker file is written at
   `<state_dir>/state.json.corrupt` (or
   `state_meta.corrupt`).
4. The daemon exits with the `StateCorrupt` error and a
   non-zero exit code.

The daemon refuses to call the GitHub API while a
corruption marker is present.

### Recovery workflow (JSON)

Recovery is a sequence, not a single command. Do not skip
steps.

1. **Stop the daemon.** Whichever path you used to start
   it (Hermes cron, system cron, manual invocation), kill
   the active tick. The daemon's whole-tick flock handles
   in-flight locks, but new ticks would race your recovery.

2. **Read the marker file.** The marker file is empty; its
   presence is the signal. `cat
   $STATE_DIR/state.json.corrupt` (or
   `state_meta.corrupt`) will return immediately.

3. **Read the archive.** The original file lives at
   `state.json.corrupt-<ts>` (or the metadata equivalent).
   Open it; understand what's wrong. The most common causes
   are:
   - A half-written file from a crash mid-write (the
     atomic-write discipline should prevent this; if you
     see it, file a bug).
   - Operator hand-edit (the daemon never does this; see
     the warning at the top of this doc).
   - Filesystem corruption (rare; check `dmesg` for I/O
     errors).

4. **Build a repaired file.** The repaired file must be a
   valid envelope:
   - `state.json` must parse as the `QueueState` schema
     (`entries` as a map of display keys to `QueueEntry`
     records).
   - `state_meta.json` must parse as the `StateMeta`
     schema.
   If you don't know how to write that by hand, the easiest
   path is to run `caduceus migrate-state --from <file>`
   against a prior-state JSON file (see `MIGRATION.md`).

5. **Apply the repaired file.** There are two paths here;
   pick the one that fits the situation:
   - **Library API:** if you have a Rust binary at hand and
     want the safe, daemon-lock-protected path, call
     `caduceus::migrate::recover_state(
     repaired_path, state_dir, /*clear_marker=*/ true,
     /*hold_daemon_lock=*/ true)`. The function archives
     the corrupt original, atomically installs the repaired
     file, and only then clears the corruption marker.
     The canonical source for this API is
     `src/state/migrate.rs`.
   - **Direct install:** if you understand what you're
     doing, manually move the corrupt file aside (rename
     `state.json` → `state.json.corrupt-<your-ts>`), write
     the repaired content with the canonical temp+fsync+
     rename pattern, and remove the corruption marker with
     `rm <state_dir>/state.json.corrupt`. **This path
     bypasses the daemon-lock protection and the library's
     archive logic.** Use the library path if you can.

6. **Verify.** `caduceus status --json` should report the
   recovered state and a clean `state_corrupt: false`. If
   it doesn't, do not push; the recovery didn't take.

7. **Restart the daemon.**

## The `migrate-state` Subcommand

```text
caduceus migrate-state [--to-sqlite] [--from <file>] [--dry-run]
```

- **`caduceus migrate-state --to-sqlite`** — Migrates the
  active JSON state (`state.json`, `state_meta.json`) into
  the SQLite store (`state.db`). Preserves the original
  JSON files as a validated backup. Updates the
  `state_backend` config field to `"sqlite"`.
- **`caduceus migrate-state --from <file>`** — Imports a
  JSON-formatted state file from a prior installation into
  the current schema. Documented in detail in `MIGRATION.md`.

This is *not* the same as recovery; recovery is for
in-place corruption, migration is for cross-format import.

If your `<state_dir>/state.db` already has an up-to-date
schema and no JSON files remain to import, migration is
not needed.

## The `queue reset` Subcommand

```text
caduceus queue reset owner/repo#number [--dry-run] [--force-finalization-reset]
```

The recovery operation for a `Failed` or `Skipped` entry.
Moves the entry back to `Queued`. The persisted
`FinalizationCheckpoint` (branch / PR / run ID / commit
OID) is preserved by default so a follow-up tick resumes
from the saved state. `--force-finalization-reset` drops
the checkpoint and the daemon prints a warning listing the
branch and PR URL; the daemon never deletes remote
branches or PRs.

The subcommand takes the daemon lock and refuses entries
with an active claim file. **Removing and re-adding the
trigger label is not a substitute** for this command; the
budget of three total worker attempts is preserved across
label churn and only the explicit reset path clears it.

## Backup Retention

- **After `migrate-state --to-sqlite`**, the original JSON
  files are preserved at their original paths as validated
  backups. They are not automatically removed.
- **SQLite backups** created during recovery
  (`state.db.bak-<ts>`, `state.db.corrupt-<ts>`) are never
  automatically swept. Operators can remove old backups
  manually:

  ```bash
  # keep the most recent 5
  ls -t $STATE_DIR/state.db.bak-* | tail -n +6 | xargs rm -f
  ```

- **JSON backups** from pre-migration recovery
  (`state.json.bak-<ts>`) follow the same pattern:

  ```bash
  # keep the most recent 5
  ls -t $STATE_DIR/state.json.bak-* | tail -n +6 | xargs rm -f
  ```

A retention sweep inside the daemon is a future feature;
the operator is responsible for housekeeping.

## When to File a Bug

- The daemon reported a `StateCorrupt` error for a
  `state.db` that passes `PRAGMA integrity_check` (this
  means the daemon's loader had a false positive; file
  with `state.db` attached).
- The daemon reported a `StateCorrupt` error for a JSON
  file whose content is parseable as the current schema
  (same — false positive; file with the archive attached).
- The daemon refused a recovery that, in your judgement,
  was valid (attach both the corrupted original and your
  repaired file or SQL script).
- The recovery succeeded but the daemon's behaviour on the
  next tick was wrong (attach the recovered state and the
  relevant tick log).

In all cases, file at the project's GitHub issues. Do not
include secrets.
