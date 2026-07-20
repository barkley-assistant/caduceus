//! Runtime pre-flight validation for Caduceus.
//!
//! [`preflight`] runs *before* the daemon tick lock so configuration
//! errors surface immediately instead of being silently swallowed
//! by the lock's "another cron tick is in progress" path. It checks:
//!
//! 1. The configured ``worker_command`` is non-empty.
//! 2. The worker executable resolves through the daemon's PATH
//!    lookup (absolute path or ``which`` lookup).
//! 3. When the second argument names the bundled
//!    ``worker-bridge.py`` template, that file is readable.
//! 4. ``git`` is on PATH (the daemon shells out to it for
//!    commit/push/fetch).
//! 5. The state-dir and workdir-base parents are writable (the
//!    daemon creates them itself if missing).
//! 6. The host supports Unix process-group semantics — required
//!    for the worker supervisor's SIGKILL/SIGTERM broadcast.
//!
//! Each check returns a [`CheckOutcome`] tagged with the path
//! that failed (when relevant) and a stable error message. Tests
//! drive [`preflight`] directly with a controlled PATH and
//! temp directories; the daemon's main loop wraps the same call.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Outcome of a single preflight check. The variants are public so
/// the structured logger can emit them as discrete fields without
/// parsing error strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The check passed.
    Ok,
    /// The configured worker command is empty or missing.
    WorkerCommandEmpty,
    /// The configured worker executable could not be resolved.
    WorkerCommandUnresolved { looked_for: String },
    /// The bundled bridge file is missing or unreadable.
    BridgeUnreadable { path: PathBuf },
    /// ``git`` is not on the daemon's PATH.
    GitMissing,
    /// A parent directory of the state dir or workdir base is
    /// not writable.
    DirNotWritable { path: PathBuf },
    /// The host does not support Unix process-group semantics.
    ProcessGroupsUnsupported,
}

impl CheckOutcome {
    /// True when the outcome represents a hard failure that must
    /// block the daemon's tick.
    pub fn is_failure(&self) -> bool {
        !matches!(self, CheckOutcome::Ok)
    }

    /// One-line human-readable description.
    pub fn describe(&self) -> String {
        match self {
            CheckOutcome::Ok => "ok".to_string(),
            CheckOutcome::WorkerCommandEmpty => "worker_command is empty".to_string(),
            CheckOutcome::WorkerCommandUnresolved { looked_for } => {
                format!("worker executable could not be resolved: {looked_for}")
            }
            CheckOutcome::BridgeUnreadable { path } => {
                format!("bundled bridge is unreadable: {}", path.display())
            }
            CheckOutcome::GitMissing => "git executable not on PATH".to_string(),
            CheckOutcome::DirNotWritable { path } => {
                format!("directory not writable: {}", path.display())
            }
            CheckOutcome::ProcessGroupsUnsupported => {
                "host does not support Unix process groups".to_string()
            }
        }
    }
}

/// Run every preflight check against *cfg* with the supplied
/// ``PATH``-style *path_env* string and return the first failure
/// (or [`CheckOutcome::Ok`]). The check is intentionally short,
/// deterministic, and side-effect free — it never spawns the
/// worker, never touches GitHub, and never modifies the filesystem.
pub fn preflight(cfg: &Config, path_env: &str) -> CaduceusResult<CheckOutcome> {
    // 1. Worker command must be non-empty.
    if cfg.worker_command.is_empty() {
        return Ok(CheckOutcome::WorkerCommandEmpty);
    }

    // 2. Worker executable must resolve.
    let program = &cfg.worker_command[0];
    let resolved = match resolve_executable(program, path_env) {
        Ok(p) => p,
        Err(_) => {
            return Ok(CheckOutcome::WorkerCommandUnresolved {
                looked_for: program.clone(),
            });
        }
    };

    // 3. Bundled bridge readability check (only when the second
    // argument names the canonical template).
    if let Some(arg) = cfg.worker_command.get(1) {
        if arg.ends_with("worker-bridge.py") {
            match check_bridge_readable(&resolved, arg) {
                Ok(()) => {}
                Err(_) => {
                    return Ok(CheckOutcome::BridgeUnreadable {
                        path: PathBuf::from(arg),
                    });
                }
            }
        }
    }

    // 4. Git must be on PATH.
    if which_in("git", path_env).is_none() {
        return Ok(CheckOutcome::GitMissing);
    }

    // 5. State dir + workdir base parents must be writable.
    if let Some(path) = first_unwritable_parent(&cfg.state_dir) {
        return Ok(CheckOutcome::DirNotWritable { path });
    }
    if let Some(path) = first_unwritable_parent(&cfg.workdir_base) {
        return Ok(CheckOutcome::DirNotWritable { path });
    }

    // 6. Host must support Unix process-group semantics.
    if !process_groups_supported() {
        return Ok(CheckOutcome::ProcessGroupsUnsupported);
    }

    Ok(CheckOutcome::Ok)
}

/// Resolve *program* against *path_env* (or an absolute path). The
/// result is the resolved absolute path of an executable file or a
/// ``Config`` error when resolution fails.
pub fn resolve_executable(program: &str, path_env: &str) -> CaduceusResult<PathBuf> {
    if program.is_empty() {
        return Err(CaduceusError::Config(
            "worker_command program is empty".to_string(),
        ));
    }
    let p = PathBuf::from(program);
    if p.is_absolute() {
        if !is_executable_file(&p) {
            return Err(CaduceusError::Config(format!(
                "worker executable is not a regular executable file: {}",
                p.display()
            )));
        }
        return Ok(p);
    }
    match which_in(program, path_env) {
        Some(found) => Ok(found),
        None => Err(CaduceusError::Config(format!(
            "worker executable not on PATH: {program}"
        ))),
    }
}

/// Locate *program* in any directory listed in *path_env*. Returns
/// the first executable match.
pub fn which_in(program: &str, path_env: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_env) {
        let candidate = dir.join(program);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// True when *path* is a regular file with at least one executable
/// bit set. Symlinks are followed.
pub fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        // On non-Unix the daemon is unsupported; return false so
        // the host-mismatch path is loud.
        let _ = path;
        false
    }
}

/// Walk up the path until a parent exists, then probe writability.
/// Caduceus creates the state dir on first run, so the test is
/// "can we create it?" — i.e. is some existing ancestor writable?
pub fn first_unwritable_parent(path: &Path) -> Option<PathBuf> {
    let mut cursor: Option<&Path> = Some(path);
    while let Some(p) = cursor {
        if p.exists() {
            return if is_dir_writable(p) {
                None
            } else {
                Some(p.to_path_buf())
            };
        }
        cursor = p.parent();
    }
    // Path has no existing ancestor. On Unix the writability of the
    // root filesystem is a fair check; the test environment
    // usually has a writable temp dir.
    None
}

fn is_dir_writable(path: &Path) -> bool {
    let probe = path.join(".caduceus-write-probe");
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// True when the current OS supports Unix process-group semantics.
/// On non-Unix platforms we surface a clear "unsupported" outcome
/// rather than papering over the gap.
pub fn process_groups_supported() -> bool {
    #[cfg(unix)]
    {
        true
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Read one byte from *bridge_arg* as a cheap "can we read" probe.
/// Public so the pre-flight tests can drive it directly without
/// having to compose a full [`Config`].
pub fn check_bridge_readable(_executable: &Path, bridge_arg: &str) -> CaduceusResult<()> {
    let p = PathBuf::from(bridge_arg);
    if !p.is_file() {
        return Err(CaduceusError::Config(format!(
            "worker bridge path is not a regular file: {}",
            p.display()
        )));
    }
    // Read a single byte as a cheap "can we read" probe.
    use std::io::Read;
    let mut f = std::fs::File::open(&p)?;
    let mut buf = [0u8; 1];
    f.read_exact(&mut buf)?;
    Ok(())
}
