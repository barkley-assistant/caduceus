//! Pure argv builder for Docker and Podman CLI invocations.
//!
//! This module is intentionally free of `tokio::process::Command` -- the
//! subprocess boundary lives in the oci_lifecycle module. Every function
//! is a pure transformation: same inputs -> same outputs.

use std::path::{Path, PathBuf};

use crate::executor::ExecutorSpec;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};

// ---------------------------------------------------------------------------
// OciEngine
// ---------------------------------------------------------------------------

/// Which OCI CLI engine the argv is being built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OciEngine {
    Docker,
    Podman,
}

impl OciEngine {
    /// Determine the engine from the binary name.
    pub fn from_binary_name(name: &str) -> Self {
        let file_name = Path::new(name)
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        if file_name == "podman" {
            OciEngine::Podman
        } else {
            OciEngine::Docker
        }
    }
}

// ---------------------------------------------------------------------------
// MountSpec
// ---------------------------------------------------------------------------

/// A single bind-mount declaration for a container.
#[derive(Clone, Debug)]
pub struct MountSpec {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub read_only: bool,
}

// ---------------------------------------------------------------------------
// build_argv
// ---------------------------------------------------------------------------

/// Build the argv vector for an OCI CLI `run` command.
///
/// Returns `OciUndeclaredMount` when the spec's worktree path is not
/// covered by any entry in `mounts` (AC-02).
pub fn build_argv(
    spec: &ExecutorSpec,
    cfg: &Config,
    mounts: &[MountSpec],
    secret_env_file: Option<&Path>,
) -> CaduceusResult<Vec<String>> {
    let _engine = OciEngine::from_binary_name(&cfg.oci_cli.to_string_lossy());
    let cli = cfg.oci_cli.to_string_lossy().to_string();
    let mut argv = vec![
        cli,
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        spec.run_id.clone(),
    ];

    // Validate and emit -v flags
    let worktree_str = spec.worktree.to_string_lossy();
    let worktree_mounted = mounts.iter().any(|m| {
        let host = m.host_path.to_string_lossy();
        host == worktree_str
    });
    if !worktree_mounted {
        return Err(CaduceusError::OciUndeclaredMount {
            path: spec.worktree.to_string_lossy().to_string(),
        });
    }

    for mount in mounts {
        let mode = if mount.read_only { "ro" } else { "rw" };
        argv.push("-v".to_string());
        argv.push(format!(
            "{}:{}:{}",
            mount.host_path.display(),
            mount.container_path.display(),
            mode,
        ));
    }

    // --env-file <secret_path> (if secrets provided)
    if let Some(secret_path) = secret_env_file {
        argv.push("--env-file".to_string());
        argv.push(secret_path.to_string_lossy().to_string());
    }

    // -e CADUCEUS_RUN_ID / CADUCEUS_ISSUE_ID
    argv.push("-e".to_string());
    argv.push(format!("CADUCEUS_RUN_ID={}", spec.run_id));
    argv.push("-e".to_string());
    argv.push(format!("CADUCEUS_ISSUE_ID={}", spec.issue.display_key()));

    // -l labels
    let daemon_id = derive_daemon_id(cfg);
    argv.push("-l".to_string());
    argv.push(format!("caduceus.daemon_id={daemon_id}"));
    argv.push("-l".to_string());
    argv.push(format!("caduceus.run_id={}", spec.run_id));
    argv.push("-l".to_string());
    argv.push(format!("caduceus.issue_id={}", spec.issue.display_key()));

    // --entrypoint <worker_command[0]>
    if let Some(entrypoint) = spec.worker_command.first() {
        argv.push("--entrypoint".to_string());
        argv.push(entrypoint.clone());
    }

    // <image>@<digest>
    let image = format!("{}@{}", derive_image_name(cfg), cfg.oci_image_digest);
    argv.push(image);

    // <worker_command[1..]>
    for arg in spec.worker_command.iter().skip(1) {
        argv.push(arg.clone());
    }

    Ok(argv)
}

/// Derive a stable daemon identifier from the config.
/// Uses the state_dir as a proxy — this is guaranteed stable for the
/// lifetime of a daemon instance.
fn derive_daemon_id(cfg: &Config) -> String {
    cfg.state_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Derive the image name (without digest) from config.
/// Defaults to `"caduceus-worker"` when not otherwise configured.
fn derive_image_name(_cfg: &Config) -> String {
    // Task 6.3 will add a configurable image name. For now we
    // hardcode a sensible default.
    "caduceus-worker".to_string()
}
