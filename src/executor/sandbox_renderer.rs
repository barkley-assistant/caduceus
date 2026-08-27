//! Pure deterministic renderer: `SandboxSpec + SandboxEngine -> create argv`.
//!
//! [`render`] / [`render_with_env_files`] are the **sole** producers of
//! OCI `create` argv in the crate. They are pure functions of their
//! arguments: no `tokio::process`, no `std::fs`, no `std::env`, no
//! global mutable state, no error cases. For identical inputs the
//! output is byte-identical on every call.
//!
//! The image token sits at a structural position (after the last `-l`
//! label and after `--entrypoint`), so no content-sniffing of the
//! image token is ever needed downstream.
//!
//! Container paths are never invented here: the workspace and output
//! mount targets (`/workspace`, `/output`) and every host path come
//! from the resolved [`SandboxSpec`].

use std::path::PathBuf;

use crate::executor::sandbox_spec::{NetworkMode, SandboxEngine, SandboxSpec};

/// Render the full `create` argv for the given engine with no secret
/// env files. Delegates to [`render_with_env_files`] with an empty
/// slice.
pub fn render(spec: &SandboxSpec, engine: SandboxEngine) -> Vec<String> {
    render_with_env_files(spec, engine, &[])
}

/// Render the full `create` argv for the given engine, appending each
/// ephemeral secret env-file path in slice order after the tmpfs
/// mounts and before the `-e` environment entries.
///
/// `env_files` are paths only — created after resolution
/// (`secret_transport::EphemeralSecretFile`), so they never live in
/// the spec and the spec stays snapshot-pure.
pub fn render_with_env_files(
    spec: &SandboxSpec,
    engine: SandboxEngine,
    env_files: &[PathBuf],
) -> Vec<String> {
    let mut argv = Vec::new();

    argv.push(engine.binary_name().to_string());
    argv.push("create".to_string());

    // --- Fixed security controls (from FixedSecurityPolicy's existence).
    argv.push("--user".to_string());
    argv.push(format!("{}:{}", spec.identity().uid, spec.identity().gid));
    argv.push("--cap-drop".to_string());
    argv.push("ALL".to_string());
    argv.push("--security-opt".to_string());
    argv.push("no-new-privileges".to_string());
    argv.push("--read-only".to_string());

    // --- Per-engine deltas, encoded strictly inside the match so the
    // compiler forces both branches.
    match engine {
        SandboxEngine::Docker => {
            // Default namespace: `--user 1000:1000` is exact, no
            // userns flag needed.
        }
        SandboxEngine::Podman => {
            // Rootless Podman (>= 4.3 mapping syntax): map container
            // uid/gid 1000 to the invoking host user so `--user
            // 1000:1000` remains meaningful. Under rootful podman the
            // mapping is identity, so one flag is correct in both modes.
            argv.push("--userns".to_string());
            argv.push(format!(
                "keep-id:uid={},gid={}",
                spec.identity().uid,
                spec.identity().gid
            ));
        }
    }

    // --- Network mode.
    argv.push("--network".to_string());
    argv.push(match spec.network() {
        NetworkMode::None => "none".to_string(),
        NetworkMode::Unrestricted => "host".to_string(),
    });

    // --- Resource limits (total fields — always emitted).
    argv.push("--cpus".to_string());
    argv.push(format!("{}", spec.resources().cpus));
    argv.push("--memory".to_string());
    argv.push(format!("{}m", spec.resources().memory_mb));
    argv.push("--pids-limit".to_string());
    argv.push(format!("{}", spec.resources().pids));
    argv.push("--shm-size".to_string());
    argv.push(format!("{}m", spec.resources().shm_mb));

    // --- Container name.
    argv.push("--name".to_string());
    argv.push(spec.name().to_string());

    // --- Mounts: exactly one workspace mount and one output mount.
    // The container targets are the fixed canonical constants the
    // resolver chose (`/workspace`, `/output`); the renderer formats
    // them from the spec without inventing any path.
    argv.push("-v".to_string());
    argv.push(format!(
        "{}:/workspace:{}",
        spec.workspace_mount().host_path.display(),
        mode(spec.workspace_mount().read_only)
    ));
    argv.push("-v".to_string());
    argv.push(format!(
        "{}:/output:{}",
        spec.output_mount().host_path.display(),
        mode(spec.output_mount().read_only)
    ));

    // --- Tmpfs mounts (in spec order).
    for mount in spec.tmpfs() {
        argv.push("--tmpfs".to_string());
        argv.push(format!("{}:size={}m", mount.target, mount.size_mb));
    }

    // --- Secret env files (in slice order).
    for path in env_files {
        argv.push("--env-file".to_string());
        argv.push(path.display().to_string());
    }

    // --- Environment, read from the spec (not `std::env`).
    // `resolve` guarantees both CADUCEUS_* entries; the fallbacks are
    // a non-panicking safety net for a spec-construction bug.
    argv.push("-e".to_string());
    argv.push(format!(
        "CADUCEUS_RUN_ID={}",
        env_value(spec, "CADUCEUS_RUN_ID", spec.name().to_string())
    ));
    argv.push("-e".to_string());
    argv.push(format!(
        "CADUCEUS_ISSUE_ID={}",
        env_value(
            spec,
            "CADUCEUS_ISSUE_ID",
            label_value(spec, "caduceus.issue_id").unwrap_or_default(),
        )
    ));

    // --- Labels (spec order — fixed: daemon_id, run_id, issue_id).
    for (key, value) in spec.labels() {
        argv.push("-l".to_string());
        argv.push(format!("{key}={value}"));
    }

    // --- Entrypoint + image + worker args.
    // The image sits at a deterministic structural position — after
    // the last `-l` label and after `--entrypoint` — so no positional
    // splicing is ever needed.
    let command = spec.command();
    if let Some(entrypoint) = command.first() {
        argv.push("--entrypoint".to_string());
        argv.push(entrypoint.clone());
    }
    argv.push(spec.image().as_ref().to_string());
    for arg in command.iter().skip(1) {
        argv.push(arg.clone());
    }

    argv
}

/// Render the rw/ro suffix for a mount.
fn mode(read_only: bool) -> &'static str {
    if read_only {
        "ro"
    } else {
        "rw"
    }
}

/// Look up an environment value in the spec. `resolve` guarantees the
/// CADUCEUS_* entries; the fallback only fires on a spec-construction
/// bug and is guarded by a debug assertion (never panics).
fn env_value(spec: &SandboxSpec, key: &str, fallback: String) -> String {
    match spec.environment().iter().find(|(k, _)| k == key) {
        Some((_, value)) => value.clone(),
        None => {
            debug_assert!(false, "resolve guarantees {key} in spec.environment");
            fallback
        }
    }
}

/// Look up a label value in the spec.
fn label_value(spec: &SandboxSpec, key: &str) -> Option<String> {
    spec.labels()
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, value)| value.clone())
}
