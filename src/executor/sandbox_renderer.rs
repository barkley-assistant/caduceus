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
use crate::worker::worker_contract::{
    CONTAINER_OUTPUT_PATH, CONTAINER_WORKSPACE_PATH, WORKER_RESULT_FILE,
};

/// Pinned engine-log rotation cap per rotated segment (`--log-opt
/// max-size`). Both engines' default drivers (Docker `json-file`,
/// Podman `k8s-file`) accept these options; no explicit
/// `--log-driver` is emitted so the renderer stays engine-agnostic.
pub const OCI_LOG_MAX_SIZE: &str = "10m";

/// Pinned engine-log segment count (`--log-opt max-file`). Worst-case
/// per-container on-disk logs: 3 × 10 MiB = 30 MiB.
pub const OCI_LOG_MAX_FILE: &str = "3";

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

    // --- Identity, encoded entirely in the spec by `resolve`'s
    //     (engine, engine_mode) matrix. Rootful modes emit
    //     `--user <owner-uid>:<owner-gid>`; rootless modes emit no
    //     `--user` (the engine's user namespace already maps the
    //     in-container identity to the unprivileged engine user, so
    //     no host-root privilege is granted). There is no hard-coded
    //     identity anywhere in the renderer.
    let identity = spec.identity();
    if identity.emit_user {
        argv.push("--user".to_string());
        argv.push(format!("{}:{}", identity.uid, identity.gid));
    }
    argv.push("--cap-drop".to_string());
    argv.push("ALL".to_string());
    argv.push("--security-opt".to_string());
    argv.push("no-new-privileges".to_string());
    argv.push("--read-only".to_string());

    // --- User namespace, also encoded in the spec: the only value
    //     ever stored is plain `keep-id` (Podman rootless), so the
    //     in-container identity equals the invoking user — the daemon
    //     user, which is the worktree owner. No uid=/gid= mapping is
    //     ever emitted.
    if let Some(userns) = identity.userns {
        argv.push("--userns".to_string());
        argv.push(userns.to_string());
    }

    // --- Network mode. Host networking is structurally
    //     unrepresentable: `NetworkMode` has a single variant, so
    //     `--network none` is emitted unconditionally (issue #245).
    argv.push("--network".to_string());
    argv.push(match spec.network() {
        NetworkMode::None => "none".to_string(),
    });

    // --- Resource limits (total fields — always emitted).
    argv.push("--cpus".to_string());
    argv.push(format!("{}", spec.resources().cpus));
    argv.push("--memory".to_string());
    argv.push(format!("{}m", spec.resources().memory_mb));
    // Swap is pinned EQUAL to the memory limit: for both engines
    // `memory-swap == memory` means total (mem + swap) equals the
    // memory limit, i.e. swap cannot double committed memory.
    argv.push("--memory-swap".to_string());
    argv.push(format!("{}m", spec.resources().memory_mb));
    argv.push("--pids-limit".to_string());
    argv.push(format!("{}", spec.resources().pids));
    // `/dev/shm` is declared via the dual tmpfs list from the spec
    // (same bounded-size mechanism as `/tmp`); the standalone
    // `--shm-size` flag was removed as redundant.

    // --- Bounded engine logs. Both entries are emitted for every
    //     run on both engines, without an explicit `--log-driver`:
    //     each engine's default driver (Docker `json-file`, Podman
    //     `k8s-file`) accepts `max-size` / `max-file`, so no
    //     per-engine branch is needed.
    argv.push("--log-opt".to_string());
    argv.push(format!("max-size={OCI_LOG_MAX_SIZE}"));
    argv.push("--log-opt".to_string());
    argv.push(format!("max-file={OCI_LOG_MAX_FILE}"));

    // --- Container name.
    argv.push("--name".to_string());
    argv.push(spec.name().to_string());

    // --- Mounts: the two writable host-backed surfaces (`/workspace`,
    //     `/output`) plus the optional read-only `.git` shadow at
    //     `/workspace/.git`. The container targets are the fixed
    //     canonical constants the resolver chose; the renderer
    //     formats them from the spec without inventing any path.
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
    // The shadow is emitted after the `/workspace` bind. Docker and
    // Podman (crun) order mounts by target depth and apply a bind
    // whose target is nested under another bind over the parent bind
    // — `/workspace/.git` wins over `/workspace` regardless of argv
    // order; the deterministic ordering matches that depth rule.
    if let Some(shadow) = spec.git_shadow() {
        argv.push("-v".to_string());
        argv.push(format!(
            "{}:/workspace/.git:{}",
            shadow.host_path.display(),
            mode(shadow.read_only)
        ));
    }

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
    // Remaining canonical `CADUCEUS_*` entries, in canonical order.
    // `resolve` guarantees all of them; the fallbacks are a
    // non-panicking safety net for a spec-construction bug.
    let result_path_fallback = format!("{CONTAINER_OUTPUT_PATH}/{WORKER_RESULT_FILE}");
    for (key, fallback) in [
        ("CADUCEUS_ISSUE_NUMBER", ""),
        ("CADUCEUS_ISSUE_REPO", ""),
        ("CADUCEUS_ISSUE_TITLE", ""),
        ("CADUCEUS_ISSUE_BODY", ""),
        ("CADUCEUS_ISSUE_LABELS_JSON", "[]"),
        ("CADUCEUS_CONTEXT_JSON", "{}"),
        ("CADUCEUS_BRANCH_NAME", ""),
        ("CADUCEUS_WORKTREE_PATH", CONTAINER_WORKSPACE_PATH),
        ("CADUCEUS_RESULT_PATH", result_path_fallback.as_str()),
    ] {
        argv.push("-e".to_string());
        argv.push(format!(
            "{key}={}",
            env_value(spec, key, fallback.to_string())
        ));
    }

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
