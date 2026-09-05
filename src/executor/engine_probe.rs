//! Pre-flight I/O probe for the OCI executor.
//!
//! This module is the **only** new I/O surface introduced by the
//! workspace/git-identity isolation change. It collects every
//! filesystem- or engine-derived fact that [`resolve`](crate::executor::sandbox_spec::resolve)
//! needs while staying pure (no `std::fs`, no `std::env` inside the
//! resolver/renderer) and creates the daemon-owned host artifacts the
//! container will bind-mount:
//!
//! 1. Worktree owner uid/gid — `std::fs::metadata` +
//!    `MetadataExt::{uid, gid}`.
//! 2. Host `.git` type at `<worktree>/.git` — `symlink_metadata`
//!    → `GitShadowKind::{File, Dir, Absent}`.
//! 3. Engine mode (rootful/rootless) + userns-remap flag — a bounded
//!    `docker info` / `podman info` call parsed by the pure
//!    [`parse_engine_mode`]. This is the only engine-CLI call added
//!    here; it must NOT move into `oci_lifecycle`, which stays a
//!    lifecycle-only boundary.
//! 4. Daemon-owned artifacts under `cfg.state_dir`:
//!    `<state_dir>/oci-runs/<run_id>/` (fresh per run, throwaway),
//!    the `output` directory (mode `0700`, chowned to the resolved
//!    identity), and the `.git` shadow artifact per kind.
//! 5. The assembled extended [`RuntimeFacts`].
//!
//! Every failure is fail-closed with
//! [`CaduceusError::OciIdentityUnsupported`]: the daemon never
//! guesses an engine mode and never falls back to a hard-coded
//! identity. Refusals happen **before** any `create` argv exists, so
//! an unsupported configuration can never reach `oci_lifecycle`.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use nix::unistd::{chown, Gid, Uid};

use crate::executor::sandbox_spec::{EngineMode, GitShadowKind, RuntimeFacts, SandboxEngine};
use crate::executor::ExecutorSpec;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Sentinel content of the `.git` shadow file. It deliberately
/// contains no real gitdir path: git commands inside the container
/// fail on the bogus gitdir, which is the intended outcome (repo
/// operations belong to the host finalize step).
pub const GIT_SHADOW_FILE_CONTENT: &str = "gitdir: /caduceus-git-shadow/unavailable\n";

/// Bounded probe timeout: a hung engine CLI fails fast instead of
/// stalling the daemon tick.
const ENGINE_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Collect all pre-flight runtime facts and create the daemon-owned
/// host artifacts for the run. Called by `OciExecutor::run` **before**
/// `resolve` — nothing downstream may perform I/O.
///
/// Fail-closed: worktree unreadable, `.git` unprobeable, engine-mode
/// probe failure, unsupported namespace configuration, or
/// output-dir/shadow creation failure all return
/// [`CaduceusError::OciIdentityUnsupported`] before any container is
/// created.
pub async fn probe_runtime_facts(
    cfg: &Config,
    spec: &ExecutorSpec,
) -> CaduceusResult<RuntimeFacts> {
    let meta = if cfg.state_backend == "sqlite" {
        crate::state::meta::MetaStore::open_sqlite(&cfg.state_dir)?
    } else {
        crate::state::meta::MetaStore::open(&cfg.state_dir)?
    };
    let daemon_id = meta.get_or_create_installation_uuid()?;
    probe_runtime_facts_with_daemon_id(cfg, spec, &daemon_id).await
}

/// Variant used by the daemon after it has loaded its persisted installation
/// UUID. Keeping the identity an explicit input ensures labels are never
/// rendered before the UUID has been durably established.
pub async fn probe_runtime_facts_with_daemon_id(
    cfg: &Config,
    spec: &ExecutorSpec,
    daemon_id: &str,
) -> CaduceusResult<RuntimeFacts> {
    let engine = cfg.sandbox().engine;

    // 1. Worktree owner — a missing/unreadable worktree is
    //    fail-closed: there is no identity to run the worker as.
    let meta = std::fs::metadata(&spec.worktree).map_err(|e| {
        identity_unsupported(
            engine,
            None,
            format!("worktree {} is not readable: {e}", spec.worktree.display()),
        )
    })?;
    let worktree_uid = meta.uid();
    let worktree_gid = meta.gid();

    // 2. Host `.git` type. A symlink counts as `File` (linked
    //    worktrees may symlink the pointer); a directory as `Dir`;
    //    `NotFound` as `Absent`; any other probe error is
    //    fail-closed.
    let git_shadow_kind = match std::fs::symlink_metadata(spec.worktree.join(".git")) {
        Ok(md) if md.is_dir() => GitShadowKind::Dir,
        Ok(_) => GitShadowKind::File,
        Err(e) if e.kind() == io::ErrorKind::NotFound => GitShadowKind::Absent,
        Err(e) => {
            return Err(identity_unsupported(
                engine,
                None,
                format!(
                    "cannot probe .git in worktree {}: {e}",
                    spec.worktree.display()
                ),
            ))
        }
    };

    // 3. Engine-mode probe (bounded timeout; fail-closed).
    let stdout = engine_info_output(engine).await?;
    let (engine_mode, userns_remap) = parse_engine_mode(engine, &stdout)?;

    // 4. Unsupported-namespace refusal — raised before any `create`
    //    argv exists. userns-remap under rootful is the canonical
    //    unsupported case; userns-remap does not occur under rootless
    //    (rootless already implies its own user namespace).
    if engine_mode == EngineMode::Rootful && userns_remap {
        return Err(identity_unsupported(
            engine,
            Some(engine_mode),
            "rootful engine configured with userns-remap is unsupported".to_string(),
        ));
    }

    // 5. Daemon-owned artifacts under the state directory. The
    //    run-scoped `oci-runs/<run_id>` subtree is created fresh per
    //    run and is throwaway; it shares the state dir's existing
    //    lifecycle and needs no new teardown code.
    let state_dir = cfg.state_dir.clone();
    let run_dir = state_dir.join("oci-runs").join(&spec.run_id);
    std::fs::create_dir_all(&run_dir).map_err(|e| {
        identity_unsupported(
            engine,
            Some(engine_mode),
            format!("cannot create run dir {}: {e}", run_dir.display()),
        )
    })?;

    let output_dir = run_dir.join("output");
    create_output_dir(&output_dir, worktree_uid, worktree_gid, engine, engine_mode)?;

    let git_shadow_host = run_dir.join("git-shadow");
    create_git_shadow(&git_shadow_host, git_shadow_kind, engine, engine_mode)?;

    // 6. Assemble the extended facts.
    let target = spec.target.display();
    Ok(RuntimeFacts {
        run_id: spec.run_id.clone(),
        target,
        worker_command: spec.worker_command.clone(),
        worktree: spec.worktree.clone(),
        output_dir,
        daemon_id: daemon_id.to_string(),
        workdir_base: cfg.workdir_base.clone(),
        state_dir,
        worktree_uid,
        worktree_gid,
        engine_mode,
        git_shadow_kind,
        git_shadow_host,
    })
}

/// Pure engine-mode parser. Unit-tested against the synthetic stdout
/// shapes below; unparseable output is fail-closed
/// (`mode: None` — the daemon never guesses).
///
/// - **Podman**: `podman info --format '{{.Host.Security.Rootless}}'`
///   prints `true`/`false`.
/// - **Docker**: `docker info --format '{{.SecurityOptions}}'`
///   prints e.g. `[name=apparmor name=seccomp,profile=builtin]`,
///   `[name=rootless ...]`, or `[name=userns-remap,...]`.
///   Substring matching (not exact tokenization) is used because the
///   `SecurityOptions` formatting varies across Docker versions.
pub fn parse_engine_mode(
    engine: SandboxEngine,
    stdout: &str,
) -> CaduceusResult<(EngineMode, bool /* userns_remap */)> {
    let unparseable = || {
        identity_unsupported(
            engine,
            None,
            "cannot determine engine rootful/rootless mode from engine \
             info output"
                .to_string(),
        )
    };
    let trimmed = stdout.trim();
    match engine {
        SandboxEngine::Podman => match trimmed {
            "true" => Ok((EngineMode::Rootless, false)),
            "false" => Ok((EngineMode::Rootful, false)),
            _ => Err(unparseable()),
        },
        SandboxEngine::Docker => {
            if trimmed.contains("name=rootless") {
                Ok((EngineMode::Rootless, false))
            } else if trimmed.contains("name=userns-remap") {
                Ok((EngineMode::Rootful, true))
            } else if trimmed == "[]" || trimmed.contains("name=") {
                // A recognized (possibly empty) SecurityOptions list
                // without rootless/userns-remap: plain rootful.
                Ok((EngineMode::Rootful, false))
            } else {
                Err(unparseable())
            }
        }
    }
}

/// Run the bounded engine-mode probe. The only new engine-CLI call in
/// the crate; failure, timeout, or non-zero exit are fail-closed.
async fn engine_info_output(engine: SandboxEngine) -> CaduceusResult<String> {
    let mut cmd = tokio::process::Command::new(engine.binary_name());
    cmd.arg("info").arg("--format").arg(match engine {
        SandboxEngine::Docker => "{{.SecurityOptions}}",
        SandboxEngine::Podman => "{{.Host.Security.Rootless}}",
    });
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let output = tokio::time::timeout(ENGINE_PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            identity_unsupported(
                engine,
                None,
                "engine info probe timed out; engine mode cannot be determined".to_string(),
            )
        })?
        .map_err(|e| {
            identity_unsupported(
                engine,
                None,
                format!("engine info probe failed to spawn: {e}"),
            )
        })?;

    if !output.status.success() {
        return Err(identity_unsupported(
            engine,
            None,
            format!("engine info probe exited with {}", output.status),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Create the daemon-owned `/output` host directory: mode `0700`,
/// chowned to the resolved `(uid, gid)`. In the supported
/// configurations the daemon user *is* the worktree owner (the daemon
/// created the worktree), so the chown is normally a no-op; a rootful
/// daemon serving a differently-owned worktree gets correct ownership
/// via the chown. An unprivileged daemon facing a foreign-owned
/// worktree cannot chown, and the run is refused fail-closed.
fn create_output_dir(
    path: &Path,
    uid: u32,
    gid: u32,
    engine: SandboxEngine,
    mode: EngineMode,
) -> CaduceusResult<()> {
    std::fs::create_dir_all(path).map_err(|e| {
        identity_unsupported(
            engine,
            Some(mode),
            format!("cannot create output dir {}: {e}", path.display()),
        )
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
        identity_unsupported(
            engine,
            Some(mode),
            format!("cannot set mode on output dir {}: {e}", path.display()),
        )
    })?;
    chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))).map_err(|e| {
        identity_unsupported(
            engine,
            Some(mode),
            format!(
                "cannot chown output dir {} to {uid}:{gid}: {e} \
                 (the resolved identity cannot own its writable surfaces)",
                path.display()
            ),
        )
    })?;
    Ok(())
}

/// Create the `.git` shadow host artifact per [`GitShadowKind`]:
///
/// - `File`: a regular file, mode `0644`, containing
///   [`GIT_SHADOW_FILE_CONTENT`]. World-readable on purpose: the
///   worker (arbitrary resolved uid) must be able to *read* the
///   shadow, and the content contains no real gitdir path.
/// - `Dir`: an empty directory, mode `0755` (readable, not writable
///   through the read-only mount). A host-backed empty dir is chosen
///   over a tmpfs so both engines use one uniform `-v …:ro`
///   mechanism.
/// - `Absent`: create nothing — the repo is unaffected.
fn create_git_shadow(
    path: &Path,
    kind: GitShadowKind,
    engine: SandboxEngine,
    mode: EngineMode,
) -> CaduceusResult<()> {
    match kind {
        GitShadowKind::File => {
            std::fs::write(path, GIT_SHADOW_FILE_CONTENT).map_err(|e| {
                identity_unsupported(
                    engine,
                    Some(mode),
                    format!("cannot create .git shadow file {}: {e}", path.display()),
                )
            })?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).map_err(
                |e| {
                    identity_unsupported(
                        engine,
                        Some(mode),
                        format!("cannot set mode on .git shadow {}: {e}", path.display()),
                    )
                },
            )?;
        }
        GitShadowKind::Dir => {
            std::fs::create_dir_all(path).map_err(|e| {
                identity_unsupported(
                    engine,
                    Some(mode),
                    format!("cannot create .git shadow dir {}: {e}", path.display()),
                )
            })?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(
                |e| {
                    identity_unsupported(
                        engine,
                        Some(mode),
                        format!("cannot set mode on .git shadow {}: {e}", path.display()),
                    )
                },
            )?;
        }
        GitShadowKind::Absent => {}
    }
    Ok(())
}

/// Build the typed fail-closed refusal.
fn identity_unsupported(
    engine: SandboxEngine,
    mode: Option<EngineMode>,
    reason: String,
) -> CaduceusError {
    CaduceusError::OciIdentityUnsupported {
        engine,
        mode,
        reason,
    }
}
