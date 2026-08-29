//! Daemon-private OCI env-file transport (issue #249; design D5).
//!
//! The assembled OCI worker environment travels as ONE `--env-file`
//! passed to `create` — never as `-e` argv tokens, so no environment
//! value can appear in argv. [`OciEnvFile`] owns the file lifecycle:
//!
//! - **Location**: the daemon-private run directory
//!   (`state_dir/oci-runs/<run_id>`), never shared `std::env::temp_dir()`.
//! - **Name**: `caduceus_env_<random>.env`, generated from the crate's
//!   existing ULID randomness source (the same one that mints run
//!   IDs) — non-deterministic, not derivable from run inputs.
//! - **Creation**: `OpenOptions::create_new` + `mode(0o600)` — the
//!   handle returned is the one written to (no reopen, no TOCTOU
//!   follow); a name collision is retried with a fresh random suffix,
//!   bounded.
//! - **Contents**: one sorted `KEY=VALUE` line per entry with a
//!   trailing newline. Newline-bearing values cannot be represented
//!   in the line-based env-file format, so they are rejected here
//!   with a typed error naming the variable — never its value —
//!   BEFORE any container exists. Canonical `CADUCEUS_*` values are
//!   newline-normalized upstream at resolution (`sandbox_spec`),
//!   so this rejection is the fail-closed backstop for operator
//!   `pass_env` values, which are never normalized (design D3).
//! - **Deletion**: `Drop` removes the file; the lifecycle guard
//!   (`oci_lifecycle::run_with_argv`) drops the handle immediately
//!   after the `create` CLI call returns on every path (success,
//!   create-failure, cancellation), so `start`/`wait`/`stop`/`rm` run
//!   with the values already gone from disk.
//!
//! `Debug` prints the path only — never the contents (the contents
//! ARE the values; spec R6).

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::infra::error::{CaduceusError, CaduceusResult};

/// File-name prefix of the daemon-private OCI env file.
const FILE_PREFIX: &str = "caduceus_env_";

/// File-name suffix of the daemon-private OCI env file.
const FILE_SUFFIX: &str = ".env";

/// Bounded `create_new` collision retries: the random suffix is a
/// fresh 128-bit ULID per attempt, so even a bound this small is
/// astronomically unlikely to exhaust.
const MAX_NAME_ATTEMPTS: usize = 16;

/// Handle to the daemon-private env file carrying the assembled OCI
/// environment. Not `Clone` — exactly one owner deletes the file.
///
/// `Drop` deletes the file on every exit path (including unwind);
/// deletion is best-effort and idempotent (a file already removed
/// elsewhere is not an error).
pub struct OciEnvFile {
    path: PathBuf,
}

impl OciEnvFile {
    /// Write `env` as sorted `KEY=VALUE` lines into a fresh
    /// mode-`0600` env file inside `run_dir` (created mode `0700` if
    /// absent) and return the owning handle.
    ///
    /// Fails with a typed error BEFORE any container exists when the
    /// run dir cannot be created, the file cannot be opened
    /// exclusively, or any value contains a newline (the env-file
    /// format is line-based and cannot represent one).
    pub fn create(run_dir: &Path, env: &BTreeMap<String, String>) -> CaduceusResult<Self> {
        Self::create_with(run_dir, env, || ulid::Ulid::new().to_string())
    }

    /// Test seam: [`create`] with a deterministic candidate-name
    /// sequence, so the `create_new` collision-retry path can be
    /// exercised without depending on ULID collisions. Names are
    /// consumed in order; exhausting them fails with a typed error.
    #[doc(hidden)]
    pub fn create_with_names_for_tests(
        run_dir: &Path,
        env: &BTreeMap<String, String>,
        names: &[String],
    ) -> CaduceusResult<Self> {
        let mut index = 0usize;
        Self::create_with(run_dir, env, || {
            let name = names
                .get(index)
                .cloned()
                .unwrap_or_else(|| ulid::Ulid::new().to_string());
            index += 1;
            name
        })
    }

    /// Shared creation path. `next_name` yields candidate random
    /// suffixes; `create_new` rejects an already-existing name and
    /// the loop retries with the next candidate, bounded.
    fn create_with(
        run_dir: &Path,
        env: &BTreeMap<String, String>,
        mut next_name: impl FnMut() -> String,
    ) -> CaduceusResult<Self> {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(run_dir)
            .map_err(|e| {
                CaduceusError::Other(format!(
                    "cannot create OCI run dir {}: {e}",
                    run_dir.display()
                ))
            })?;

        // One `KEY=VALUE` line per entry, sorted keys (BTreeMap
        // iteration order), trailing newline.
        let mut body = String::new();
        for (key, value) in env {
            if value.contains('\n') || value.contains('\r') {
                // A newline-bearing value cannot survive the
                // line-based env-file format: fail closed, naming the
                // variable — never its value (design D3/D5).
                return Err(CaduceusError::Config(format!(
                    "environment variable {key} contains a newline and \
                     cannot be transported in the OCI env file"
                )));
            }
            body.push_str(key);
            body.push('=');
            body.push_str(value);
            body.push('\n');
        }

        for _ in 0..MAX_NAME_ATTEMPTS {
            let name = format!("{FILE_PREFIX}{}{FILE_SUFFIX}", next_name());
            let path = run_dir.join(&name);
            let mut file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => file,
                // Collision on the random suffix — retry with a fresh
                // candidate (bounded).
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(CaduceusError::Other(format!(
                        "cannot create OCI env file {}: {e}",
                        path.display()
                    )));
                }
            };
            file.write_all(body.as_bytes()).map_err(|e| {
                CaduceusError::Other(format!("cannot write OCI env file {}: {e}", path.display()))
            })?;
            return Ok(Self { path });
        }
        Err(CaduceusError::Other(format!(
            "cannot create a unique OCI env-file name in {} after \
             {MAX_NAME_ATTEMPTS} attempts",
            run_dir.display()
        )))
    }

    /// The env-file path — the only value ever exposed (and the only
    /// thing about this file that may appear in argv, as the
    /// `--env-file` argument).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for OciEnvFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Path only — never contents (spec R6; design D7).
        f.debug_struct("OciEnvFile")
            .field("path", &self.path)
            .finish()
    }
}

impl Drop for OciEnvFile {
    fn drop(&mut self) {
        // Best-effort and idempotent: a file already removed (or one
        // whose creation failed midway) is not an error.
        let _ = std::fs::remove_file(&self.path);
    }
}
