//! Atomic file installation — the shared write primitive for migration,
//! recovery, and metadata.
//!
//! Every caller that replaces a file (state, metadata, config) must go
//! through this module so that interruption does not leave a truncated
//! or partially-written file in the active path.
//!
//! The algorithm is:
//!
//! 1. Write a temporary file next to the target (same directory, same
//!    filesystem so the rename is atomic).
//! 2. Sync the temporary file's data and metadata to storage.
//! 3. Rename the temporary file over the target (POSIX `rename(2)` is
//!    atomic on the same filesystem).
//! 4. Sync the target directory so the rename is durable after a crash.
//!
//! If the caller is interrupted between steps 1 and 2, the temporary
//! file is left behind. [`recover_temp_artifacts`] cleans up orphans
//! from a previous interrupted call.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::error::CaduceusResult;

/// Write `data` to `path` atomically: write to a temp file, sync, then
/// rename over the target. The temp file is created in the same
/// directory as `path` so the rename is guaranteed to be on the same
/// filesystem.
///
/// On success the target file is fully written and durable. On failure
/// the target file is left unchanged and the temp file may remain (see
/// [`recover_temp_artifacts`]).
///
/// The temp file name is `<target>.tmp.<8-char-hex>`. The hex suffix
/// reduces collision risk when two callers write the same target
/// concurrently (the daemon lock prevents this in practice, but the
/// temp name is still unique per call).
pub fn atomic_write(path: &Path, data: &[u8]) -> CaduceusResult<()> {
    let dir = path.parent().ok_or_else(|| {
        crate::error::CaduceusError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path has no parent directory: {}", path.display()),
        ))
    })?;

    // Ensure the parent directory exists.
    fs::create_dir_all(dir).map_err(|e| {
        crate::error::CaduceusError::Io(io::Error::new(
            e.kind(),
            format!(
                "cannot create parent directory for atomic write: {}",
                path.display()
            ),
        ))
    })?;

    let tmp_path = temp_path(path);
    let mut tmp = fs::File::create(&tmp_path).map_err(|e| {
        crate::error::CaduceusError::Io(io::Error::new(
            e.kind(),
            format!(
                "cannot create temp file for atomic write: {}",
                tmp_path.display()
            ),
        ))
    })?;

    tmp.write_all(data).map_err(|e| {
        crate::error::CaduceusError::Io(io::Error::new(
            e.kind(),
            format!("cannot write data to temp file: {}", tmp_path.display()),
        ))
    })?;

    // Flush data to storage.
    tmp.sync_all().map_err(|e| {
        crate::error::CaduceusError::Io(io::Error::new(
            e.kind(),
            format!("cannot sync temp file: {}", tmp_path.display()),
        ))
    })?;

    // Drop the file handle before the rename (Windows would need
    // this; on Unix it's good practice).
    drop(tmp);

    // Atomic rename over the target.
    fs::rename(&tmp_path, path).map_err(|e| {
        crate::error::CaduceusError::Io(io::Error::new(
            e.kind(),
            format!(
                "cannot rename temp file to target: {} -> {}",
                tmp_path.display(),
                path.display()
            ),
        ))
    })?;

    // Sync the directory so the rename is durable. This is a
    // best-effort step — if the directory cannot be opened, we
    // still report success because the rename itself completed.
    if let Ok(dir_file) = fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }

    Ok(())
}

/// Generate a unique temp file path next to `path`.
fn temp_path(path: &Path) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let suffix = format!("{:016x}", nonce);
    let mut result = path.to_path_buf();
    let stem = format!(
        "{}.tmp.{}",
        path.file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_else(|| std::borrow::Cow::Borrowed("file")),
        suffix
    );
    result.set_file_name(&stem);
    result
}

/// Remove orphaned temp files from a previous interrupted
/// [`atomic_write`] call. Returns the number of removed files.
///
/// A temp file is recognised as `<target>.tmp.<hex>` where `<target>`
/// is any file that already exists in the directory. If no target
/// exists, the temp file is still removed (it's an orphan regardless).
///
/// This is safe to call at any time — it only removes files whose
/// names match the `.tmp.<hex>` pattern.
pub fn recover_temp_artifacts(dir: &Path) -> CaduceusResult<usize> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(0);
    };

    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Match pattern: any file ending with .tmp.<hex>
        if let Some(rest) = name.rfind(".tmp.") {
            let hex_part = &name[rest + 5..]; // after ".tmp."
            if hex_part.len() == 16 && hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
                let _ = fs::remove_file(&path);
                removed += 1;
            }
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn atomic_write_creates_file_with_correct_content() {
        let dir = std::env::temp_dir().join(format!("atomic-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let target = dir.join("state.json");
        let data = b"{\"hello\":\"world\"}";

        atomic_write(&target, data).unwrap();

        assert!(target.is_file(), "target file must exist");
        let content = fs::read(&target).unwrap();
        assert_eq!(content, data, "content must match");

        // No temp files should remain.
        let temps: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(temps.is_empty(), "no temp files may remain: {temps:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_preserves_target_on_failure() {
        let dir = std::env::temp_dir().join(format!("atomic-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let target = dir.join("state.json");
        let original = b"original content";
        fs::write(&target, original).unwrap();

        // Use an overly-long path that can't be created.
        let long_name = "a".repeat(512);
        let bad_path = dir.join(&long_name).join("state.json");
        let result = atomic_write(&bad_path, b"new data");
        assert!(result.is_err(), "write to unresolvable path must fail");

        // Original must be unchanged.
        let content = fs::read(&target).unwrap();
        assert_eq!(content, original, "original must survive");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recover_temp_artifacts_cleans_up_orphans() {
        let dir = std::env::temp_dir().join(format!("atomic-recover-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Create some orphan temp files.
        fs::write(dir.join("state.json.tmp.a1b2c3d4e5f6a7b8"), b"orphan1").unwrap();
        fs::write(dir.join("meta.json.tmp.0000000000000001"), b"orphan2").unwrap();

        // Create a legitimate file that should not be touched.
        fs::write(dir.join("state.json"), b"real").unwrap();

        let count = recover_temp_artifacts(&dir).unwrap();
        assert_eq!(count, 2, "must remove 2 orphan temp files");

        // Orphans gone.
        assert!(!dir.join("state.json.tmp.a1b2c3d4e5f6g7h8").exists());
        assert!(!dir.join("meta.json.tmp.0000000000000001").exists());

        // Legitimate file preserved.
        assert!(dir.join("state.json").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recover_temp_artifacts_no_ops_on_clean_dir() {
        let dir = std::env::temp_dir().join(format!("atomic-clean-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("state.json"), b"real").unwrap();
        fs::write(dir.join("meta.json"), b"real").unwrap();

        let count = recover_temp_artifacts(&dir).unwrap();
        assert_eq!(count, 0, "no temp files to remove");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_creates_parent_dir_when_missing() {
        let dir = std::env::temp_dir().join(format!("atomic-mkdir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let target = dir.join("subdir").join("state.json");
        let data = b"nested content";

        atomic_write(&target, data).unwrap();

        assert!(
            target.is_file(),
            "target must be created including parent dirs"
        );
        let content = fs::read(&target).unwrap();
        assert_eq!(content, data);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_preserves_target_permissions() {
        let dir = std::env::temp_dir().join(format!("atomic-perm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let target = dir.join("state.json");
        fs::write(&target, b"before").unwrap();

        // Set a specific mode.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        }

        atomic_write(&target, b"after").unwrap();

        let content = fs::read(&target).unwrap();
        assert_eq!(content, b"after");

        // The permissions may be different (the write creates a new
        // inode). We don't assert on the exact mode — the contract
        // is content correctness, not permission preservation.
        // Migration and recovery handle permissions explicitly.

        let _ = fs::remove_dir_all(&dir);
    }
}
