//! Ephemeral secret file transport for OCI secrets.
//!
//! Secrets travel through a mode-0600 temporary file whose path is
//! passed as `--env-file` to the OCI CLI. The `SecretHandle`'s `Drop`
//! impl deletes the file on every exit path (success, failure, cancel,
//! panic). The secret value never appears in argv, logs, or Debug
//! output.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::infra::error::{CaduceusError, CaduceusResult};

/// Factory for creating ephemeral mode-0600 secret files.
#[derive(Debug)]
pub struct EphemeralSecretFile;

impl EphemeralSecretFile {
    /// Write each key-value pair to its own temp file with mode 0o600.
    ///
    /// Returns a `SecretHandle` holding only the file paths — never the
    /// secret values. The handle's `Drop` impl removes every file.
    pub fn write(values: &[(String, String)]) -> CaduceusResult<SecretHandle> {
        let mut paths = Vec::with_capacity(values.len());
        for (key, value) in values {
            let path = write_secret_file(key, value)?;
            paths.push(path);
        }
        Ok(SecretHandle { paths })
    }
}

/// Write a single secret to a temporary file with mode 0o600.
fn write_secret_file(key: &str, value: &str) -> CaduceusResult<PathBuf> {
    // Use the system temp dir.
    let dir = std::env::temp_dir();
    let file_name = format!("caduceus_secret_{key}.env");
    let path = dir.join(&file_name);

    let mut file = fs::File::create(&path).map_err(|e| {
        CaduceusError::Other(format!("failed to create secret file {file_name}: {e}"))
    })?;

    // Write the key=value pair.
    writeln!(file, "{key}={value}").map_err(|e| {
        CaduceusError::Other(format!("failed to write secret file {file_name}: {e}"))
    })?;

    // Set mode 0600.
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|e| {
            CaduceusError::Other(format!(
                "failed to set mode on secret file {file_name}: {e}"
            ))
        })?;

    Ok(path)
}

/// Handle to ephemeral secret files. The `Drop` impl deletes every
/// file on any exit path (including panic).
///
/// Only the file paths are stored — the secret values are never in
/// the struct, so `Debug` and `Display` cannot leak them.
#[derive(Clone)]
pub struct SecretHandle {
    paths: Vec<PathBuf>,
}

impl SecretHandle {
    /// The file paths owned by this handle.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

impl std::fmt::Debug for SecretHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretHandle")
            .field("paths", &self.paths)
            .finish()
    }
}

impl std::fmt::Display for SecretHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretHandle({} path(s))", self.paths.len())
    }
}

impl Drop for SecretHandle {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}
