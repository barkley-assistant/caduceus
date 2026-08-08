# State Recovery

The daemon owns its state. Operators own their recovery.
**Do not edit `state.json`, `state_meta.json`,
`state.db`, claim files, or transcripts in place.** The
lock and atomic-write discipline only hold for the
programmatic API. This doc is the API.

The migration procedure from a prior installation is
in the README's ["Replacing a prior install"
section](../README.md). This doc covers in-place recovery,
which is different: state has become corrupt
in-place and the daemon is refusing to start.

> **Backend selection.** New installations use the
> JSON backend (`state_backend: json`, the default).
> The SQLite backend is opt-in, enabled by running
> `caduceus migrate-state --to-sqlite` (see
> [`migrate-state` Subcommand](#the-migrate-state-subcommand)).
> Recovery differs between the two backends: JSON uses
> marker files and atomic file swap; SQLite uses
> `PRAGMA integrity_check`, backup/restore, and the
> `recover_sqlite_state` library API. Pick the section
> that matches your `state_backend`.

## The JSON Recovery Path (default backend)

JSON is the default backend. State lives in
`<state_dir>/state.json` and
`<state_dir>/state_meta.json`.

### JSON failure modes

The daemon validates both files on every `state_dir`
open. When it finds a malformed file:

1. The file is preserved at its original path (no
   silent truncation; no overwrite with empty state).
2. A timestamped archive is written at
   `<state_dir>/state.json.corrupt-<unix-ts>` (or
   `state_meta.json.corrupt-<unix-ts>`).
3. A corruption marker file is written at
   `<state_dir>/state.json.corrupt` (or
   `state_meta.corrupt`).
4. The daemon exits with the `StateCorrupt` error and
   a non-zero exit code.

The daemon refuses to call the GitHub API while a
corruption marker is present. "Use the recovery
path" — this is what we mean.

### Detecting JSON corruption

- **`caduceus status --json`:** the `state_corrupt`
  field is `true`.
- **Marker file:** `cat $STATE_DIR/state.json.corrupt`
  (or `state_meta.corrupt`) returns immediately; the
  file's presence is the signal.
- **Daemon stderr:** a `StateCorrupt` error names the
  malformed file.

### JSON recovery workflow

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
   (see [The migrate-state Subcommand](#the-migrate-state-subcommand)).
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
     aside (rename `state.json` ->
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

## The SQLite Recovery Path (opt-in backend)

SQLite is the optional backend, enabled by
`caduceus migrate-state --to-sqlite`. State lives in
`<state_dir>/state.db` (WAL mode, versioned schema).
Unlike the JSON backend, SQLite corruption surfaces
as an **open-time error** rather than a marker file:
the daemon does not write a `state.db.corrupt`
marker. Corruption is detected by a failed
`PRAGMA integrity_check`, a schema-version mismatch,
or a structurally malformed database file.

### Detecting SQLite corruption

The daemon detects SQLite corruption at open time and
surfaces it through several channels.

**From `caduceus status --json`:** The `state_corrupt`
field is **not** the SQLite signal — it reflects the
JSON-backend marker and is `false` for SQLite. SQLite
corruption instead surfaces via the top-level
`diagnostic` field, which reports `corrupt_state` or
`corrupt_queue`, or as an error that prevents
`status --json` from opening the store at all.

**From the daemon's stderr:** The daemon exits with a
`StateCorrupt` error. Common messages include:

- `SQLite store has schema v<N> but this daemon only
  supports v<M> — upgrade required` (schema version
  mismatch; authored by Caduceus).
- `cannot open SQLite store at <path>` (the file is
  not a valid SQLite database; authored by Caduceus).
- `database disk image is malformed` (the database
  file is structurally corrupt — this is the most
  common corruption signal; **this string is emitted
  by the SQLite engine itself**, wrapped into a
  `StateCorrupt` error by Caduceus, so its exact
  wording is controlled by SQLite, not the daemon).

**From SQLite directly:**

```bash
sqlite3 $STATE_DIR/state.db "PRAGMA integrity_check;"
```

A healthy database returns `ok`. Anything else is a
corruption signal.

### SQLite failure modes

The daemon validates the SQLite store on every
`state_dir` open. When it finds a corrupt database:

1. The original database file is preserved at its
   original path (no silent truncation; no overwrite
   with empty state).
2. During recovery (via `recover_sqlite_state`), the
   corrupt database is archived at
   `<state_dir>/state.db.corrupt-<unix-ts>`.
3. The daemon exits with the `StateCorrupt` error and
   a non-zero exit code. **No marker file is written
   for the SQLite backend**; if a
   `<state_dir>/state.db.corrupt` marker exists from
   an earlier or manual run, `recover_sqlite_state`
   will remove it as part of recovery.

### SQLite recovery workflow

Recovery is a sequence, not a single command. Do not
skip steps.

1. **Stop the daemon.** Whichever path you used to
   start it (Hermes cron, system cron, manual
   invocation), kill the active tick. The daemon's
   whole-tick flock handles in-flight locks, but new
   ticks would race your recovery.

2. **Check the database integrity.** Run the built-in
   integrity check to confirm and scope the
   corruption:

   ```bash
   sqlite3 $STATE_DIR/state.db "PRAGMA integrity_check;"
   ```

   If the database is intact but the daemon still
   reports a `StateCorrupt`, the problem may be
   schema version mismatch — check the version with:

   ```bash
   sqlite3 $STATE_DIR/state.db "SELECT MAX(version) FROM schema_version;"
   ```

3. **Create a safe backup.** Before any repair work,
   make a copy of the current database file:

   ```bash
   sqlite3 $STATE_DIR/state.db ".backup $STATE_DIR/state.db.backup-$(date +%s)"
   ```

   Or using `VACUUM INTO` (SQLite 3.27+):

   ```bash
   sqlite3 $STATE_DIR/state.db "VACUUM INTO '$STATE_DIR/state.db.backup-$(date +%s)';"
   ```

   A `VACUUM INTO` rejects corrupt pages; if it fails,
   the database has structural damage that needs a
   restore or surgical repair.

4. **Restore or repair.** Pick the path that fits the
   situation:

   - **Restore from a known-good backup (library API):**
     If you have a previous backup (e.g., from a cron
     `sqlite3 .backup` job), use the recovery function.
     The library API is
     `caduceus::migrate::recover_sqlite_state(
     state_dir, Some(backup_path),
     /*hold_daemon_lock=*/ true)`. The canonical
     source is `src/state/migrate.rs`. This function
     archives the corrupt file, validates the backup,
     atomically installs it, and clears the marker.

   - **Create a fresh database (library API):**
     `caduceus::migrate::recover_sqlite_state(
     state_dir, None,
     /*hold_daemon_lock=*/ true)`. The queue state is
     lost; re-enqueue needed issues manually or re-add
     trigger labels.

   - **Surgical repair (advanced):** If the corruption
     is localised to one table or a few rows, use the
     `sqlite3` CLI directly:

     ```bash
     sqlite3 $STATE_DIR/state.db "DELETE FROM queue_entries WHERE issue_key = 'owner/repo#999';"
     ```

     Or rebuild the database from `.dump`:

     ```bash
     sqlite3 $STATE_DIR/state.db ".dump" | sqlite3 $STATE_DIR/state.db.repaired
     # Stop the daemon first, then:
     mv $STATE_DIR/state.db.repaired $STATE_DIR/state.db
     ```

     **This bypasses the daemon-lock protection.** Use
     the library path if you can.

5. **Verify.** `caduceus status --json` should open
   the store cleanly (no `corrupt_state` /
   `corrupt_queue` diagnostic, and the daemon should
   start without a `StateCorrupt` error). For the
   SQLite backend the `state_corrupt` field stays
   `false` — it is not the SQLite signal. If
   verification fails, do not push; the recovery
   didn't take.

6. **Restart the daemon.**

## The `migrate-state` Subcommand

```text
caduceus migrate-state --to-sqlite [--dry-run]
caduceus migrate-state --from <file> [--dry-run]
```

Two modes (mutually exclusive):

- **`--to-sqlite`** — switches an installation from
  the default JSON backend to SQLite. Reads
  `<state_dir>/state.json` and
  `<state_dir>/state_meta.json` and imports them into
  the SQLite store at `<state_dir>/state.db`. After
  migration, the JSON files are preserved as validated
  backups. The `state_backend` configuration key is
  updated to `sqlite`.
- **`--from <file>`** — imports a JSON-formatted state
  file from a different state directory into the current
  schema. Documented in detail in
  [The migrate-state Subcommand](#the-migrate-state-subcommand).

This is *not* the same as recovery; recovery is for
in-place corruption, migration is for cross-format
import or backend switch.

If your `state_backend` is already `sqlite`,
`--to-sqlite` is not needed.

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
entries with an active claim. **Removing and
re-adding the trigger label is not a substitute** for
this command; the budget of three total worker
attempts is preserved across label churn and only the
explicit reset path clears it.

## Finalization Checkpoints

The daemon writes a durable `FinalizationCheckpoint` on
the queue entry at every code-ticket finalization stage
— `ResultValidated`, `Committed`, `Pushed`, and
`PrCreated` — in addition to the SQLite `checkpoints`
rows. Each stage persists the SQLite checkpoint first,
then the queue checkpoint, so the queue field never
advertises a stage whose SQLite row is missing. The
queue checkpoint anchors recovery to the original
`run_id`: a crash after a commit or push resumes at the
recorded stage instead of re-dispatching the worker and
creating a duplicate branch/commit (issue #119).
Investigation tickets get the same durable queue
checkpoint at `InvestigationReady` /
`InvestigationCommented` (issue #120). A crashed
finalization never needs manual queue manipulation —
see `queue reset` above for the failure-path recovery.

## Backup Retention

The **SQLite** database uses WAL mode, so when
`state_backend: sqlite` the state directory may also
contain:

- `state.db-wal` — the write-ahead log (WAL file,
  present during writes)
- `state.db-shm` — shared-memory WAL index (present
  during writes)

These are normal; do not delete them. They are
automatically checkpointed by SQLite and do not need
manual housekeeping.

The **JSON** backend does not produce WAL files. Its
migration installs write a new
`state.json.bak-<unix-ts>` to the state directory.

Corrupt-database archives (`state.db.corrupt-<ts>`,
written by `recover_sqlite_state`) and manual
backups (`state.db.backup-<ts>`) are not swept by the
daemon. Operators can `rm` old archives manually:

```bash
# keep the most recent 5 corrupt archives
ls -t $STATE_DIR/state.db.corrupt-* 2>/dev/null | tail -n +6 | xargs rm -f
```

A retention sweep inside the daemon is a future
feature; the operator is responsible for
housekeeping.

## When to File a Bug

- The daemon wrote a `state.json.corrupt-<ts>` (JSON
  backend) archive whose content parses as the current
  schema, or a `state.db.corrupt-<ts>` (SQLite
  backend) archive whose content passes
  `PRAGMA integrity_check` (either means the daemon's
  loader had a false positive; please file with the
  archive attached).
- The daemon refused a recovery that, in your
  judgement, was valid (attach both the corrupted
  original and your repaired file).
- The recovery succeeded but the daemon's behaviour on
  the next tick was wrong (attach the recovered state
  and the relevant tick log).

In all cases, file at the project's GitHub issues. Do
not include secrets.
