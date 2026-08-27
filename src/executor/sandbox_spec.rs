//! Closed typed OCI sandbox specification and its resolution step.
//!
//! [`SandboxSpec`] is a sealed struct — the only way to construct it
//! is [`resolve`], which converts [`SandboxConfig`] + [`RuntimeFacts`]
//! into a fully populated spec. All host-path, identity, mount, and
//! policy decisions are made here; the renderer (in `sandbox_renderer`)
//! only formats what the spec already contains.
//!
//! Pure module: no `tokio::process`, no `std::fs`, no global mutable
//! state. The only `std::env` access is for `pass_env` filtering inside
//! [`resolve`].

use std::ops::Deref;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::github::issue::IssueKey;
use crate::infra::config::{Config, OciPullPolicy, SandboxConfig, SandboxNetwork};
use crate::infra::error::{CaduceusError, CaduceusResult};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which OCI CLI engine the argv is being built for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxEngine {
    #[default]
    Docker,
    Podman,
}

impl SandboxEngine {
    /// CLI binary invoked for this engine.
    pub fn binary_name(&self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }

    /// Determine the engine from the binary name.
    pub fn from_binary_name(name: &str) -> Self {
        let file_name = Path::new(name)
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        if file_name == "podman" {
            SandboxEngine::Podman
        } else {
            SandboxEngine::Docker
        }
    }
}

/// Digest-pinned image reference. The only constructor (module-private
/// [`ImageRef::new`]) rejects any reference without `@sha256:`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef(String);

impl ImageRef {
    /// Module-private constructor — only accessible from `sandbox_spec.rs`.
    /// Rejects any reference without `@sha256:`.
    fn new(reference: &str) -> CaduceusResult<Self> {
        if !reference.contains("@sha256:") {
            return Err(CaduceusError::OciImageNotDigestPinned {
                reference: reference.to_string(),
            });
        }
        Ok(Self(reference.to_string()))
    }
}

impl Deref for ImageRef {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ImageRef {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Non-root identity inside the container. Always `1000:1000`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub uid: u32,
    pub gid: u32,
}

/// Fixed non-root UID and GID for the worker container.
pub const SANDBOX_UID: u32 = 1000;
pub const SANDBOX_GID: u32 = 1000;

/// A single bind-mount declaration for a container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountSpec {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub read_only: bool,
}

/// A tmpfs mount declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmpfsMount {
    pub target: String,
    pub size_mb: u64,
}

/// Network isolation mode for the container.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetworkMode {
    /// `--network none` — no network access.
    #[default]
    None,
    /// `--network host` — full host network access.
    Unrestricted,
}

/// Resource limits mapped from [`SandboxResources`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceLimits {
    pub cpus: f64,
    pub memory_mb: u64,
    pub pids: u64,
    pub tmpfs_mb: u64,
    pub shm_mb: u64,
}

/// Fixed security policy — always present, no configuration surface.
/// The renderer unconditionally emits `--cap-drop ALL`,
/// `--security-opt no-new-privileges`, and `--read-only` from the
/// existence of this type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FixedSecurityPolicy;

/// The closed typed intermediate representation for an OCI `create` argv.
///
/// Construction is only possible via [`resolve`] — no public
/// constructor, no `Default`, no builder. Every mandatory control
/// is a total (non-`Option`) field.
#[derive(Clone, Debug)]
pub struct SandboxSpec {
    name: String,
    image: ImageRef,
    command: Vec<String>,
    identity: ResolvedIdentity,
    workspace_mount: MountSpec,
    output_mount: MountSpec,
    tmpfs: Vec<TmpfsMount>,
    environment: Vec<(String, String)>,
    resources: ResourceLimits,
    network: NetworkMode,
    security: FixedSecurityPolicy,
    labels: Vec<(String, String)>,
}

// --- SandboxSpec accessors ------------------------------------------------

impl SandboxSpec {
    /// Container name (= run_id).
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Digest-pinned image reference.
    pub fn image(&self) -> &ImageRef {
        &self.image
    }
    /// Worker command argv.
    pub fn command(&self) -> &[String] {
        &self.command
    }
    /// Resolved identity (uid/gid).
    pub fn identity(&self) -> ResolvedIdentity {
        self.identity
    }
    /// The single workspace mount (host → `/workspace`).
    pub fn workspace_mount(&self) -> &MountSpec {
        &self.workspace_mount
    }
    /// The single output mount (host → `/output`).
    pub fn output_mount(&self) -> &MountSpec {
        &self.output_mount
    }
    /// Tmpfs mounts (ordered).
    pub fn tmpfs(&self) -> &[TmpfsMount] {
        &self.tmpfs
    }
    /// Environment entries (ordered). `resolve` guarantees
    /// `CADUCEUS_RUN_ID` and `CADUCEUS_ISSUE_ID` entries.
    pub fn environment(&self) -> &[(String, String)] {
        &self.environment
    }
    /// Resource limits.
    pub fn resources(&self) -> ResourceLimits {
        self.resources
    }
    /// Network mode.
    pub fn network(&self) -> NetworkMode {
        self.network
    }
    /// Fixed security policy (unit type — always present).
    pub fn security(&self) -> FixedSecurityPolicy {
        self.security
    }
    /// Labels (ordered: daemon_id, run_id, issue_id).
    pub fn labels(&self) -> &[(String, String)] {
        &self.labels
    }
}

// --- Module-private constructor --------------------------------------------

impl SandboxSpec {
    /// Module-private constructor. Only callable from within
    /// `sandbox_spec.rs` (i.e., by [`resolve`]).
    #[allow(clippy::too_many_arguments)]
    fn new(
        name: String,
        image: ImageRef,
        command: Vec<String>,
        identity: ResolvedIdentity,
        workspace_mount: MountSpec,
        output_mount: MountSpec,
        tmpfs: Vec<TmpfsMount>,
        environment: Vec<(String, String)>,
        resources: ResourceLimits,
        network: NetworkMode,
        security: FixedSecurityPolicy,
        labels: Vec<(String, String)>,
    ) -> Self {
        Self {
            name,
            image,
            command,
            identity,
            workspace_mount,
            output_mount,
            tmpfs,
            environment,
            resources,
            network,
            security,
            labels,
        }
    }
}

// ---------------------------------------------------------------------------
// RuntimeFacts — inputs to the resolution step
// ---------------------------------------------------------------------------

/// Runtime facts that the daemon knows before resolution: what to run,
/// where files live, and what the daemon identity is.
///
/// `engine` is deliberately **not** a field here — it lives only in
/// [`SandboxConfig`] and is passed directly to the renderer, keeping
/// a single source of truth.
#[derive(Clone, Debug)]
pub struct RuntimeFacts {
    /// Unique run identifier.
    pub run_id: String,
    /// The issue key being worked on.
    pub issue: IssueKey,
    /// Worker command argv (bridge script + args).
    pub worker_command: Vec<String>,
    /// Host path to the worktree root.
    pub worktree: PathBuf,
    /// Host path to the output directory (sibling of the worktree
    /// under the same parent, e.g. `<workdir_base>/<owner>/<repo>/result`).
    pub output_dir: PathBuf,
    /// Stable daemon identifier (state-dir basename).
    pub daemon_id: String,
    /// The declared worktree root (`Config.workdir_base`). The
    /// host-path allow-list in `resolve` rejects mounts outside
    /// this root.
    pub workdir_base: PathBuf,
}

// ---------------------------------------------------------------------------
// Resolution step
// ---------------------------------------------------------------------------

/// Resolve a [`SandboxConfig`] plus runtime facts into a closed
/// [`SandboxSpec`].
///
/// Pure — no I/O. The only `std::env` reads are for `pass_env`
/// filtering (task 1.13 contract).
pub fn resolve(sandbox: &SandboxConfig, runtime: &RuntimeFacts) -> CaduceusResult<SandboxSpec> {
    // 1. Host-path allow-list.
    //    Lexically normalize both paths so we can check containment
    //    without filesystem access (resolve is pure).
    let workdir_base_norm = lexical_normalize(&runtime.workdir_base).ok_or_else(|| {
        CaduceusError::OciUndeclaredMount {
            path: runtime.workdir_base.display().to_string(),
        }
    })?;
    if !workdir_base_norm.is_absolute() {
        return Err(CaduceusError::OciUndeclaredMount {
            path: workdir_base_norm.display().to_string(),
        });
    }

    let worktree_norm = resolve_path(&runtime.worktree, &workdir_base_norm)?;
    let output_norm = resolve_path(&runtime.output_dir, &workdir_base_norm)?;

    // 2. Workspace/output overlap check — the old double-RW mount
    //    bug (same host path at two container paths) is rejected.
    if output_norm.starts_with(&worktree_norm) || worktree_norm.starts_with(&output_norm) {
        return Err(CaduceusError::OciMountConflict {
            detail: format!(
                "output_dir {} must not equal or contain worktree {} \
                 (the old double-RW mount bug: each needs a distinct host path)",
                output_norm.display(),
                worktree_norm.display(),
            ),
        });
    }

    // 3. Mounts — exactly one workspace mount and one output mount,
    //    with fixed canonical container paths.
    let workspace_mount = MountSpec {
        host_path: worktree_norm,
        container_path: PathBuf::from("/workspace"),
        read_only: false,
    };
    let output_mount = MountSpec {
        host_path: output_norm,
        container_path: PathBuf::from("/output"),
        read_only: false,
    };

    // 4. Identity — fixed non-root uid/gid.
    let identity = ResolvedIdentity {
        uid: SANDBOX_UID,
        gid: SANDBOX_GID,
    };

    // 5. Image — reject non-digest-pinned references.
    let image = ImageRef::new(&sandbox.image)?;

    // 6. Pull policy — `Always` is incompatible with digest-pinned images.
    if sandbox.pull_policy == OciPullPolicy::Always {
        return Err(CaduceusError::OciPullPolicyIncompatible {
            detail: "pull_policy 'Always' is incompatible with \
                     digest-pinned images; use 'IfMissing' or 'Never'"
                .to_string(),
        });
    }

    // 7. Resources — mapped 1:1 from SandboxResources.
    let resources = ResourceLimits {
        cpus: sandbox.resources.cpus,
        memory_mb: sandbox.resources.memory_mb,
        pids: sandbox.resources.pids,
        tmpfs_mb: sandbox.resources.tmpfs_mb,
        shm_mb: sandbox.resources.shm_mb,
    };

    // 8. Network — map from SandboxNetwork.
    let network = match sandbox.network {
        SandboxNetwork::None => NetworkMode::None,
        SandboxNetwork::Unrestricted => NetworkMode::Unrestricted,
    };

    // 9. Environment — ordered.
    let mut environment: Vec<(String, String)> = vec![
        ("CADUCEUS_RUN_ID".to_string(), runtime.run_id.clone()),
        ("CADUCEUS_ISSUE_ID".to_string(), runtime.issue.display_key()),
    ];
    for name in &sandbox.pass_env {
        if let Ok(value) = std::env::var(name) {
            environment.push((name.clone(), value));
        }
    }

    // 10. Tmpfs — one /tmp mount sized from config.
    let tmpfs = vec![TmpfsMount {
        target: "/tmp".to_string(),
        size_mb: sandbox.resources.tmpfs_mb,
    }];

    // 11. Labels — fixed order.
    let labels = vec![
        ("caduceus.daemon_id".to_string(), runtime.daemon_id.clone()),
        ("caduceus.run_id".to_string(), runtime.run_id.clone()),
        ("caduceus.issue_id".to_string(), runtime.issue.display_key()),
    ];

    // 12. Command — worker argv.
    let command = runtime.worker_command.clone();

    // 13. Name — container name == run_id.
    let name = runtime.run_id.clone();

    Ok(SandboxSpec::new(
        name,
        image,
        command,
        identity,
        workspace_mount,
        output_mount,
        tmpfs,
        environment,
        resources,
        network,
        FixedSecurityPolicy,
        labels,
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive a stable daemon identifier from the config.
/// Uses the state-dir basename — guaranteed stable for the lifetime
/// of a daemon instance.
pub(crate) fn derive_daemon_id(cfg: &Config) -> String {
    cfg.state_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Lexically normalize an absolute path: resolve `.` and `..`
/// components without touching the filesystem. Returns `None` for
/// relative paths or paths that attempt to escape above the root.
fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::RootDir | Component::Prefix(_) => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    // Tried to escape above the root.
                    return None;
                }
            }
            Component::Normal(_) => out.push(comp.as_os_str()),
        }
    }
    Some(out)
}

/// Resolve a host path: lexical-normalize, check absolute, check
/// under `workdir_base_norm`. Returns the normalized path on success
/// or `OciUndeclaredMount` on failure.
fn resolve_path(path: &Path, workdir_base_norm: &Path) -> CaduceusResult<PathBuf> {
    let norm = lexical_normalize(path).ok_or_else(|| CaduceusError::OciUndeclaredMount {
        path: path.display().to_string(),
    })?;
    if !norm.starts_with(workdir_base_norm) {
        return Err(CaduceusError::OciUndeclaredMount {
            path: path.display().to_string(),
        });
    }
    Ok(norm)
}
