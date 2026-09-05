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

/// Labels shared by container creation and reconciliation discovery.
pub const OCI_DAEMON_LABEL: &str = "caduceus.daemon_id";
pub const OCI_RUN_LABEL: &str = "caduceus.run_id";
pub const OCI_ISSUE_LABEL: &str = "caduceus.issue_id";
/// Quoted because Docker/Podman Go templates parse dotted label keys as
/// field paths rather than literal map keys.
///
/// Two template families exist because `docker ps` and `docker inspect`
/// expose different template contexts: `docker ps --format` has no
/// `.Config` field and reads labels via the `.Label "key"` function,
/// while `docker inspect --format` templates against the full container
/// JSON and reads labels through `.Config.Labels` (the `.Label` function
/// is not available there).
pub const OCI_DAEMON_ID_DISCOVERY_TEMPLATE: &str = r#"{{.Label "caduceus.daemon_id"}}"#;
pub const OCI_RUN_ID_PS_TEMPLATE: &str = r#"{{.Label "caduceus.run_id"}}"#;
/// Inspect-style template: valid for `docker inspect --format`.
pub const OCI_RUN_ID_DISCOVERY_TEMPLATE: &str = r#"{{index .Config.Labels "caduceus.run_id"}}"#;

/// Render the identity labels in their canonical order.
pub fn render_labels(daemon_id: &str, run_id: &str, issue_id: &str) -> Vec<(String, String)> {
    vec![
        (OCI_DAEMON_LABEL.to_string(), daemon_id.to_string()),
        (OCI_RUN_LABEL.to_string(), run_id.to_string()),
        (OCI_ISSUE_LABEL.to_string(), issue_id.to_string()),
    ]
}

/// Render the full `create` argv for the given engine with no env
/// files. Delegates to [`render_with_env_files`] with an empty slice
/// — the `-e` fallback path (no production caller: the OCI executor
/// always supplies the daemon-private env file).
pub fn render(spec: &SandboxSpec, engine: SandboxEngine) -> Vec<String> {
    render_with_env_files(spec, engine, &[])
}

/// Render the full `create` argv for the given engine, appending each
/// env-file path in slice order after the tmpfs mounts.
///
/// Transport precedence (frozen, issue #249; design D4): when at
/// least one env file is supplied, the file is **authoritative** and
/// carries ALL OCI environment values — zero `-e` tokens are emitted,
/// so no environment value can reach argv. The `-e` fallback below
/// runs only for callers that render without env files (the plain
/// [`render`] path); production always supplies the daemon-private
/// env file (`oci_env_file::OciEnvFile`).
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

    // --- Network mode. This exhaustive `(mode, engine)` match is
    //     the only code path that can produce the `--network` token:
    //     `None` renders `--network none` on both engines;
    //     `Unrestricted` renders the engine's default isolated bridge
    //     (`bridge` — NAT'd outbound egress, no host namespace
    //     joining; the Podman token is pinned per `podman-create(1)`:
    //     "bridge: Create a network stack on the default bridge").
    //     Host networking is structurally unrepresentable: no arm can
    //     emit `host`, and adding a third `NetworkMode` or
    //     `SandboxEngine` variant fails to compile until this match
    //     is deliberately extended.
    let network_token = match (spec.network(), engine) {
        (NetworkMode::None, _) => "none",
        // Docker: the default isolated bridge (NAT'd egress).
        (NetworkMode::Unrestricted, SandboxEngine::Docker) => "bridge",
        // Podman: the default isolated bridge (NAT'd egress) — token
        // verified against `podman-create(1)`. Never `host`.
        (NetworkMode::Unrestricted, SandboxEngine::Podman) => "bridge",
    };
    argv.push("--network".to_string());
    argv.push(network_token.to_string());

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

    // --- Env files (in slice order). Exactly one daemon-private
    // env file is supplied in production; it carries the ENTIRE
    // assembled OCI environment.
    for path in env_files {
        argv.push("--env-file".to_string());
        argv.push(path.display().to_string());
    }

    // --- Environment. The `-e` fallback exists only for callers
    // that render without env files: a supplied env file is
    // authoritative (it is built from the same `spec.environment`),
    // and emitting `-e` alongside it would put environment values
    // back into argv, violating the frozen no-values-in-argv
    // invariant (issue #249; design D4).
    if env_files.is_empty() {
        // `resolve` guarantees the canonical CADUCEUS_* entries; the
        // fallbacks are a non-panicking safety net for a
        // spec-construction bug. The emitted list mirrors the
        // resolved target: a PR-target spec carries
        // `CADUCEUS_WORK_TARGET=pr` in its environment and renders
        // only the PR-shaped fallbacks; any other spec renders the
        // historical issue-shaped fallbacks (DAR §6.1, D5).
        let pr_mode = spec
            .environment()
            .iter()
            .any(|(k, v)| k == "CADUCEUS_WORK_TARGET" && v == "pr");
        argv.push("-e".to_string());
        argv.push(format!(
            "CADUCEUS_RUN_ID={}",
            env_value(spec, "CADUCEUS_RUN_ID", spec.name().to_string())
        ));
        if pr_mode {
            let result_path_fallback = format!("{CONTAINER_OUTPUT_PATH}/{WORKER_RESULT_FILE}");
            for (key, fallback) in [
                ("CADUCEUS_WORK_TARGET", "pr"),
                ("CADUCEUS_PR_NUMBER", ""),
                ("CADUCEUS_PR_REPO", ""),
                ("CADUCEUS_PR_BASE_SHA", ""),
                ("CADUCEUS_PR_HEAD_SHA", ""),
                ("CADUCEUS_CONTEXT_JSON", "{}"),
                ("CADUCEUS_WORKTREE_PATH", CONTAINER_WORKSPACE_PATH),
                ("CADUCEUS_RESULT_PATH", result_path_fallback.as_str()),
            ] {
                argv.push("-e".to_string());
                argv.push(format!(
                    "{key}={}",
                    env_value(spec, key, fallback.to_string())
                ));
            }
        } else {
            argv.push("-e".to_string());
            argv.push(format!(
                "CADUCEUS_ISSUE_ID={}",
                env_value(
                    spec,
                    "CADUCEUS_ISSUE_ID",
                    label_value(spec, "caduceus.issue_id").unwrap_or_default(),
                )
            ));
            // Remaining canonical `CADUCEUS_*` entries, in canonical
            // order. `resolve` guarantees all of them; the fallbacks
            // are a non-panicking safety net for a spec-construction
            // bug.
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
        }
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
