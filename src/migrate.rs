//! `caduceus migrate-state` and corruption-marker recovery.
//!
//! Task 9.1 fills in the legacy v0 import path, dry-run rollout,
//! corrupt-state and corrupt-metadata recovery, and the canonical
//! atomic-install sequence that protects callers from half-written
//! state. The module owns the public surface the CLI calls into:
//!
//! - [`run`] — the import path used by `caduceus migrate-state`.
//! - [`recover_state`] — the corruption-marker recovery path used
//!   after an operator supplies a repaired or generated file.
//!
//! ## Atomic install
//!
//! Both paths install the new state via the same temp-file +
//! fsync + rename pattern the rest of the daemon uses, so a crash
//! between install and rename leaves the active file untouched.
//! The prior content is preserved as `<state_dir>/state.json.bak-<ts>`
//! so the operator always has a one-step rollback target.
//!
//! ## Daemon lock
//!
//! Recovery takes the daemon lock so a tick cannot start while a
//! repaired state is being installed. The migration import path
//! also takes the daemon lock; concurrent cron ticks must observe
//! the lock short-circuit just as they do for every other
//! state-mutating command.

#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};

use crate::error::{CaduceusError, CaduceusResult};
use crate::issue::IssueKey;
use crate::queue::{
    parse_queue_state, serialize_queue_state, DaemonLock, Phase, QueueEntry, QueueState,
    StateStore, TicketType,
};

/// Outcome class of a migration run. The CLI renders a
/// human-readable summary from this; tests assert on it
/// directly so the import path is observable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// New entries were imported and the active state file was
    /// installed atomically.
    Imported {
        /// Number of legacy entries that became new queue entries.
        migrated: u64,
        /// Number of legacy entries that were rejected as duplicates.
        skipped: u64,
    },
    /// A dry-run was requested; nothing was installed.
    DryRun {
        /// Number of legacy entries that *would* be migrated.
        would_migrate: u64,
        /// Number of legacy entries that *would* be skipped.
        would_skip: u64,
    },
    /// The `--from` file is already at the current schema and
    /// produced an empty delta against the live state. No-op.
    AlreadyCurrent,
}

/// Result of [`run`]. The CLI prints a short summary; tests
/// assert on the struct directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub entries_migrated: u64,
    pub entries_skipped: u64,
    pub outcome: MigrationOutcome,
}

/// Result of [`recover_state`]. The CLI renders the archived
/// corrupt path so the operator can inspect it.
#[derive(Debug, Clone)]
pub struct RecoveryReport {
    /// Path to the archive copy of the original corrupt file.
    pub archived_corrupt: Option<PathBuf>,
    /// Path the active state file was installed from.
    pub installed_from: PathBuf,
    /// True when the corruption marker was removed.
    pub cleared_marker: bool,
}

/// Legacy v0 entry shape. The migration path tolerates a
/// permissive parser because the legacy processor emitted these
/// fields without `deny_unknown_fields`.
#[derive(Debug, Clone, serde::Deserialize)]
struct LegacyEntryV0 {
    repo: String,
    number: u64,
    status: String,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct LegacyStateV0 {
    #[serde(default)]
    entries: Vec<LegacyEntryV0>,
}

/// Run the migration import path. Imports the legacy v0 state
/// in `from` into a current-schema `state.json` rooted at
/// `state_dir`. A `dry_run` performs every read but never
/// installs.
pub fn run(from: &Path, state_dir: &Path, dry_run: bool) -> CaduceusResult<MigrationReport> {
    if !from.exists() {
        return Err(CaduceusError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("--from {} does not exist", from.display()),
        )));
    }
    let bytes = fs::read(from).map_err(|err| CaduceusError::StateCorrupt {
        path: from.to_path_buf(),
        message: format!("read legacy state: {err}"),
    })?;
    if bytes.is_empty() {
        return Err(CaduceusError::StateCorrupt {
            path: from.to_path_buf(),
            message: "legacy state file is empty".to_string(),
        });
    }
    let trimmed = bytes
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .copied()
        .unwrap_or(b'\0');
    if trimmed != b'{' && trimmed != b'[' {
        return Err(CaduceusError::StateCorrupt {
            path: from.to_path_buf(),
            message: "legacy state is not a JSON object/array".to_string(),
        });
    }

    // Try the current v1 schema first; fall back to v0. The
    // schemas differ at the `entries` field: v1 is a map
    // `display_key -> QueueEntry`; v0 is an array of
    // `{repo, number, status, ...}` records. Try v0 first when
    // the input looks like a v0 envelope (an array-shaped
    // `entries`), otherwise v1.
    let input_is_v0_envelope = looks_like_v0(&bytes);
    let mut target: QueueState = if input_is_v0_envelope {
        QueueState::empty()
    } else {
        match parse_v1(&bytes) {
            Ok(state) => state,
            Err(err) => {
                return Err(err);
            }
        }
    };
    let legacy_entries: Vec<LegacyEntryV0> = if input_is_v0_envelope {
        parse_v0(&bytes)?.entries
    } else {
        Vec::new()
    };
    let was_v1 = !input_is_v0_envelope;

    // The live state is authoritative for anything already
    // present in `target`. Anything already in the live state
    // is left alone and counted as skipped when the legacy
    // file would have introduced it.
    let _lock = if !dry_run {
        let lock = DaemonLock::try_acquire(state_dir)?.ok_or_else(|| CaduceusError::Queue {
            context: "migrate",
            stderr: "another tick holds daemon.lock; refusing to migrate".to_string(),
        })?;
        Some(lock)
    } else {
        None
    };

    let live = if dry_run {
        QueueState::empty()
    } else {
        // Open the store so the existing queue file (if any)
        // is validated before we touch it. The state file may
        // not exist yet (first migration); that's fine.
        if state_dir.join("state.json").exists() {
            StateStore::open(state_dir)?.snapshot()?
        } else {
            QueueState::empty()
        }
    };

    let mut migrated: u64 = 0;
    let mut skipped: u64 = 0;
    let now = Utc::now();

    if !was_v1 {
        for legacy in legacy_entries {
            // Legacy v0 uses a single `repo` field of the form
            // `owner/repo`; map it to IssueKey::parse by
            // appending the number.
            let ref_string = format!("{}#{}", legacy.repo, legacy.number);
            let key = match IssueKey::parse(&ref_string) {
                Ok(k) => k,
                _ => {
                    skipped += 1;
                    continue;
                }
            };
            if target.entries.contains_key(&key.display_key()) {
                skipped += 1;
                continue;
            }
            if live.entries.contains_key(&key.display_key()) {
                skipped += 1;
                continue;
            }
            let phase = match legacy.status.as_str() {
                "queued" => Phase::Queued,
                "in_progress" => Phase::InProgress,
                "previewed" => Phase::Previewed,
                "done" => Phase::Done,
                "failed" => Phase::Failed,
                "skipped" => Phase::Skipped,
                _ => Phase::Queued,
            };
            let ticket_type = if key.number % 2 == 0 {
                TicketType::Investigation
            } else {
                TicketType::Code
            };
            let updated_at = parse_legacy_timestamp(legacy.updated_at.as_deref(), now);
            let entry = QueueEntry {
                key: key.clone(),
                phase,
                ticket_type,
                attempts: legacy.attempts,
                last_error: legacy.last_error,
                last_run_id: None,
                next_attempt_at: None,
                finalization: None,
                queued_at: updated_at,
                updated_at,
                generation: 1,
            };
            target.entries.insert(key.display_key(), entry);
            migrated += 1;
        }
    } else {
        // For an already-v1 input, "migrated" means "entries
        // present in the input but missing from the live
        // state". When every input entry already exists in
        // the live state with identical content, the
        // migration is a no-op and we surface AlreadyCurrent.
        for (k, entry) in target.entries.clone() {
            match live.entries.get(&k) {
                Some(existing) if existing == &entry => {
                    // Identical entry already in live state.
                    target.entries.remove(&k);
                    // Note: this is an `identical-skip`,
                    // not a conflict-skip. We do NOT
                    // increment `skipped` here because the
                    // operator's intent ("the input matches
                    // the live state, do nothing") is a true
                    // no-op. Conflicts (different content for
                    // the same key) ARE counted as skipped so
                    // the operator sees them.
                }
                Some(_) => {
                    // Same key, different content. The
                    // migration refuses to overwrite; the
                    // operator must use the explicit recovery
                    // path.
                    target.entries.remove(&k);
                    skipped += 1;
                }
                None => {
                    let _ = entry;
                    migrated += 1;
                }
            }
        }
    }

    // An empty delta against the live state means the
    // migration is a no-op regardless of the source shape.
    // First-time migration (state.json doesn't exist) is
    // *not* a no-op — we still install an empty v1 envelope.
    if !dry_run && migrated == 0 && skipped == 0 && state_dir.join("state.json").exists() {
        return Ok(MigrationReport {
            entries_migrated: 0,
            entries_skipped: 0,
            outcome: MigrationOutcome::AlreadyCurrent,
        });
    }

    if dry_run {
        return Ok(MigrationReport {
            entries_migrated: migrated + skipped,
            entries_skipped: 0,
            outcome: MigrationOutcome::DryRun {
                would_migrate: migrated,
                would_skip: skipped,
            },
        });
    }

    // Install: write a temp file in the state_dir, fsync, rename,
    // and emit a backup of the prior content (if any).
    install_state(&target, state_dir)?;
    Ok(MigrationReport {
        entries_migrated: migrated,
        entries_skipped: skipped,
        outcome: MigrationOutcome::Imported { migrated, skipped },
    })
}

/// Recovery path for a corrupt `state.json` + marker. Validates
/// `repaired` as the current schema, takes the daemon lock
/// (unless `hold_daemon_lock` is `false`, which lets the caller
/// provide its own RAII guard from a test), archives the
/// corrupt original to `<state_dir>/state.json.corrupt-<ts>`,
/// atomically installs the repaired content, and only then
/// clears the corruption marker.
pub fn recover_state(
    repaired: &Path,
    state_dir: &Path,
    clear_marker: bool,
    hold_daemon_lock: bool,
) -> CaduceusResult<RecoveryReport> {
    let _lock = if hold_daemon_lock {
        let lock = DaemonLock::try_acquire(state_dir)?.ok_or_else(|| CaduceusError::Queue {
            context: "recover",
            stderr: "another tick holds daemon.lock; refusing to recover".to_string(),
        })?;
        Some(lock)
    } else {
        None
    };

    let bytes = fs::read(repaired).map_err(|err| CaduceusError::StateCorrupt {
        path: repaired.to_path_buf(),
        message: format!("read repaired file: {err}"),
    })?;
    let parsed = parse_v1(&bytes)?;
    let active = state_dir.join("state.json");
    let marker = state_dir.join("state.json.corrupt");

    let archived = if active.exists() {
        let stamp = unix_ts();
        let archive = state_dir.join(format!("state.json.corrupt-{stamp}"));
        // Refuse if the archive cannot be written; never lose
        // the original without first landing it on disk.
        fs::rename(&active, &archive).map_err(CaduceusError::Io)?;
        Some(archive)
    } else {
        None
    };

    // Install the repaired file. If this fails, restore the
    // archive (the rename already moved the corrupt original
    // aside, so a failed install must put it back).
    if let Err(err) = install_state(&parsed, state_dir) {
        if let Some(archive) = archived.as_ref() {
            let _ = fs::rename(archive, &active);
        }
        return Err(err);
    }

    let mut cleared = false;
    if clear_marker && marker.exists() {
        fs::remove_file(&marker)?;
        cleared = true;
    }

    Ok(RecoveryReport {
        archived_corrupt: archived,
        installed_from: repaired.to_path_buf(),
        cleared_marker: cleared,
    })
}

fn parse_v1(bytes: &[u8]) -> CaduceusResult<QueueState> {
    let text = std::str::from_utf8(bytes).map_err(|err| CaduceusError::StateCorrupt {
        path: PathBuf::from("<queue-state>"),
        message: format!("state file is not UTF-8: {err}"),
    })?;
    parse_queue_state(text)
}

fn parse_v0(bytes: &[u8]) -> CaduceusResult<LegacyStateV0> {
    serde_json::from_slice(bytes).map_err(|err| CaduceusError::StateCorrupt {
        path: PathBuf::from("<legacy-state>"),
        message: format!("legacy state JSON parse: {err}"),
    })
}

/// Cheap structural sniff: v0 envelopes have `entries` as a
/// JSON array; v1 has `entries` as an object/map. We accept
/// `{ "entries": [...] }` as v0 even when the array is empty.
fn looks_like_v0(bytes: &[u8]) -> bool {
    // Locate the substring `"entries"` and inspect the next
    // non-whitespace byte. This is good enough for our
    // import path; a malformed envelope still falls through
    // to the v1 parser and produces a StateCorrupt error.
    let needle = b"\"entries\"";
    let Some(idx) = find_subseq(bytes, needle) else {
        return false;
    };
    let rest = &bytes[idx + needle.len()..];
    for b in rest.iter() {
        match b {
            b':' | b' ' | b'\t' | b'\n' | b'\r' => continue,
            b'[' => return true,
            _ => return false,
        }
    }
    false
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_legacy_timestamp(value: Option<&str>, fallback: DateTime<Utc>) -> DateTime<Utc> {
    value
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(fallback)
}

fn install_state(state: &QueueState, state_dir: &Path) -> CaduceusResult<()> {
    fs::create_dir_all(state_dir)?;
    let target = state_dir.join("state.json");
    let body = serialize_queue_state(state)?;
    // Same-directory temp file. The extension is `.tmp` so a
    // partial write cannot be confused with a real backup.
    let tmp = state_dir.join("state.json.tmp");
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    // If the active file already exists, write a backup of its
    // *current* content first so we always have a rollback
    // target. When the active file does not exist, we still
    // emit a backup copy of the freshly-installed content so
    // operators have a stable rollback target without
    // digging through file-system snapshots.
    let backup = state_dir.join(format!("state.json.bak-{}", unix_ts()));
    if target.exists() {
        fs::copy(&target, &backup)?;
    } else {
        // No prior content: emit the just-installed content
        // as the backup so operators always have *something*
        // to roll back to.
        fs::copy(&tmp, &backup)?;
    }
    fs::rename(&tmp, &target)?;
    Ok(())
}

fn unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
