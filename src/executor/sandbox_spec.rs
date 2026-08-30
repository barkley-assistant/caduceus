//! Closed typed OCI sandbox specification and its resolution step.
//!
//! [`SandboxSpec`] is a sealed struct — the only way to construct it
//! is [`resolve`], which converts [`SandboxConfig`] + [`RuntimeFacts`]
//! into a fully populated spec. All host-path, identity, mount, and
//! policy decisions are made here; the renderer (in `sandbox_renderer`)
//! only formats what the spec already contains.
//!
//! Pure module: no `tokio::process`, no `std::fs`, no global mutable
//! state. The only `std::env` access is the single parent-env capture
//! inside [`resolve`]; [`resolve_with_env`] takes an injected snapshot
//! instead. Every I/O-derived fact the resolver needs (worktree
//! owner uid/gid, host `.git` type, engine rootful/rootless mode) is
//! gathered by the pre-flight probe (`engine_probe`) and carried in
//! [`RuntimeFacts`].

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::ops::Deref;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::executor::ExecutorSpec;
use crate::github::issue::IssueKey;
use crate::infra::config::{SandboxConfig, SandboxNetwork};
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::worker::worker_contract::{
    denied_name, CONTAINER_OUTPUT_PATH, CONTAINER_WORKSPACE_PATH, WORKER_RESULT_FILE,
};

/// The canonical `CADUCEUS_*` variable names every OCI worker
/// receives (the frozen worker-environment contract, issue #249).
/// Shared authority for `resolve_with_env` assembly and for the
/// `sandbox.pass_env` reserved-key collision check at config load.
pub const CANONICAL_ENV_KEYS: &[&str] = &[
    "CADUCEUS_RUN_ID",
    "CADUCEUS_ISSUE_ID",
    "CADUCEUS_ISSUE_NUMBER",
    "CADUCEUS_ISSUE_REPO",
    "CADUCEUS_ISSUE_TITLE",
    "CADUCEUS_ISSUE_BODY",
    "CADUCEUS_ISSUE_LABELS_JSON",
    "CADUCEUS_CONTEXT_JSON",
    "CADUCEUS_BRANCH_NAME",
    "CADUCEUS_WORKTREE_PATH",
    "CADUCEUS_RESULT_PATH",
];

/// Compat values always present in the OCI worker environment. The
/// container root filesystem is read-only with a bounded tmpfs at
/// `/tmp`, so both point there; they are never inherited from the
/// host environment.
pub const COMPAT_ENV_KEYS: &[&str] = &["HOME", "TMPDIR"];

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

/// Rootful vs rootless engine mode, detected by the pre-flight
/// engine probe (`engine_probe`) and carried in [`RuntimeFacts`].
///
/// `resolve` stays pure: the mode is a fact gathered outside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineMode {
    /// Engine runs with host root privileges (Docker daemon as root,
    /// Podman invoked by root).
    Rootful,
    /// Engine runs unprivileged (Docker rootless, Podman rootless):
    /// container root maps to the unprivileged engine user via a
    /// user namespace.
    Rootless,
}

/// What `.git` is on the host worktree, probed by the pre-flight
/// step and carried in [`RuntimeFacts`]. Determines the daemon-owned
/// `.git` shadow form (see [`select_git_shadow`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitShadowKind {
    /// `.git` is a regular file or symlink (the `gitdir:` pointer
    /// file of a linked worktree).
    File,
    /// `.git` is a directory (a normal checkout).
    Dir,
    /// `.git` does not exist.
    Absent,
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

/// Resolved container runtime identity, computed by [`resolve`]
/// from the `(engine, engine_mode, worktree-owner uid/gid)` matrix.
///
/// * `uid`/`gid` are the worktree owner's ids as probed by the
///   pre-flight step — never a hard-coded constant.
/// * `emit_user` selects whether the renderer emits
///   `--user <uid>:<gid>` (rootful modes) or nothing (rootless
///   modes, where the engine's user namespace already maps the
///   in-container identity to the engine user).
/// * `userns` carries the only supported user-namespace flag value,
///   `Some("keep-id")` for Podman rootless.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub uid: u32,
    pub gid: u32,
    pub emit_user: bool,
    pub userns: Option<&'static str>,
}

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

/// Network isolation mode for the container — a closed two-variant
/// enum.
///
/// `None` renders `--network none` (loopback-only). `Unrestricted`
/// renders the engine's default isolated bridge (`--network bridge`
/// on both Docker and Podman): NAT'd outbound egress with **no** host
/// namespace joining — it is **not** host networking. Host networking
/// is structurally unrepresentable: no variant can ever produce
/// `--network host`, and the exhaustive matches in the renderer and
/// in [`validate_no_host_escalation`] force a deliberate dual edit
/// before any third mode can exist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetworkMode {
    /// `--network none` — no network access (the default).
    #[default]
    None,
    /// The engine's default isolated bridge (`--network bridge`) —
    /// NAT'd outbound egress, never host networking.
    Unrestricted,
}

/// The single conversion site between the config layer
/// (`SandboxNetwork`) and the spec layer ([`NetworkMode`]).
/// Exhaustive by construction: a new `SandboxNetwork` variant breaks
/// this build until the spec-layer mapping is deliberately extended.
impl From<SandboxNetwork> for NetworkMode {
    fn from(value: SandboxNetwork) -> Self {
        match value {
            SandboxNetwork::None => Self::None,
            SandboxNetwork::Unrestricted => Self::Unrestricted,
        }
    }
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
///
/// `Debug` is a manual impl (not derived): `environment` is rendered
/// as the sorted KEY list only — never the values — so a spec dumped
/// through `Debug` (logs, panic messages, test failures) cannot leak
/// resolved `pass_env` values or canonical content (spec R6; design
/// D7).
#[derive(Clone)]
pub struct SandboxSpec {
    name: String,
    image: ImageRef,
    command: Vec<String>,
    identity: ResolvedIdentity,
    workspace_mount: MountSpec,
    output_mount: MountSpec,
    git_shadow: Option<MountSpec>,
    tmpfs: Vec<TmpfsMount>,
    environment: Vec<(String, String)>,
    resources: ResourceLimits,
    network: NetworkMode,
    security: FixedSecurityPolicy,
    labels: Vec<(String, String)>,
}

impl std::fmt::Debug for SandboxSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `environment` renders as the sorted key list only; resolved
        // values never reach `Debug` output (spec R6, design D7).
        let env_keys: Vec<&str> = self
            .environment
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        f.debug_struct("SandboxSpec")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("command", &self.command)
            .field("identity", &self.identity)
            .field("workspace_mount", &self.workspace_mount)
            .field("output_mount", &self.output_mount)
            .field("git_shadow", &self.git_shadow)
            .field("tmpfs", &self.tmpfs)
            .field("environment", &env_keys)
            .field("resources", &self.resources)
            .field("network", &self.network)
            .field("security", &self.security)
            .field("labels", &self.labels)
            .finish()
    }
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
    /// The configured digest-pinned image reference as a plain string.
    pub fn image_ref(&self) -> &str {
        self.image.as_ref()
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
    /// The optional daemon-owned `.git` shadow mount (host →
    /// `/workspace/.git`, read-only). `None` when the host worktree
    /// has no `.git` entry at all.
    pub fn git_shadow(&self) -> Option<&MountSpec> {
        self.git_shadow.as_ref()
    }
    /// Tmpfs mounts (ordered).
    pub fn tmpfs(&self) -> &[TmpfsMount] {
        &self.tmpfs
    }
    /// Environment entries, sorted by key. `resolve_with_env`
    /// guarantees the full canonical `CADUCEUS_*` set (run, issue,
    /// context, branch, and the container-side worktree/result
    /// paths), the two compat values (`HOME`/`TMPDIR`), and the
    /// resolved `sandbox.pass_env` entries — and nothing else.
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
        git_shadow: Option<MountSpec>,
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
            git_shadow,
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
    /// Host path to the output directory. Daemon-owned:
    /// `<state_dir>/oci-runs/<run_id>/output` (derived by the
    /// pre-flight probe, never a worktree sibling).
    pub output_dir: PathBuf,
    /// Stable installation UUID loaded from the metadata store.
    pub daemon_id: String,
    /// The declared worktree root (`Config.workdir_base`). The
    /// host-path allow-list in `resolve` requires the worktree to
    /// live under this root.
    pub workdir_base: PathBuf,
    /// Daemon state directory. Allow-list root for the daemon-owned
    /// surfaces (`output_dir`, `git_shadow_host`).
    pub state_dir: PathBuf,
    /// Worktree owner UID (pre-flight probe fact).
    pub worktree_uid: u32,
    /// Worktree owner GID (pre-flight probe fact).
    pub worktree_gid: u32,
    /// Engine mode (rootful/rootless), probed before resolution.
    pub engine_mode: EngineMode,
    /// Host `.git` type for the worktree (pre-flight probe fact).
    pub git_shadow_kind: GitShadowKind,
    /// Daemon-owned host path of the `.git` shadow artifact
    /// (`<state_dir>/oci-runs/<run_id>/git-shadow`), created by the
    /// pre-flight probe when `git_shadow_kind != Absent`.
    pub git_shadow_host: PathBuf,
}

// ---------------------------------------------------------------------------
// Resolution step
// ---------------------------------------------------------------------------

/// Type-aware `.git` shadow selection: `File` and `Dir` both yield a
/// single read-only mount of the daemon-owned shadow artifact at
/// `/workspace/.git`; `Absent` yields no shadow (the repo is
/// unaffected). The kind only affects what the pre-flight creates on
/// the host — the mount shape is uniform so both engines use one
/// `-v …:ro` mechanism.
///
/// Pure — unit-testable without I/O.
pub fn select_git_shadow(kind: GitShadowKind, shadow_host: &Path) -> Option<MountSpec> {
    match kind {
        GitShadowKind::File | GitShadowKind::Dir => Some(MountSpec {
            host_path: shadow_host.to_path_buf(),
            container_path: PathBuf::from("/workspace/.git"),
            read_only: true,
        }),
        GitShadowKind::Absent => None,
    }
}

/// Resolve a [`SandboxConfig`] plus runtime facts into a closed
/// [`SandboxSpec`].
///
/// This is a pure function: it reads only the sandbox configuration,
/// the probed runtime facts, and the executor spec. It performs no
/// I/O and no subprocess calls; the argv renderer and the lifecycle
/// runner consume its output. The only environment access is the
/// `pass_env` filter below (spec R4).
pub fn resolve(
    sandbox: &SandboxConfig,
    runtime: &RuntimeFacts,
    spec: &ExecutorSpec,
) -> CaduceusResult<SandboxSpec> {
    let parent_env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    resolve_with_env(sandbox, runtime, spec, &parent_env)
}

/// Newline normalization for canonical free-text values (issue
/// title/body, context JSON, …): the OCI env file is line-based and
/// cannot represent a newline, so a multi-line canonical value is
/// collapsed deterministically — `\r\n` and lone `\n` first, then
/// any remaining `\r`, each becoming a single space. GitHub issue
/// bodies are routinely multi-line, so this lets every real issue
/// resolve and run instead of hard-failing at env-file creation.
/// Content fidelity is unaffected: the full multi-line prompt is
/// written into the worktree by `write_prompt`, and operator
/// `pass_env` values are never normalized (they fail closed,
/// design D3).
fn normalize_canonical_value(value: &str) -> String {
    value.replace("\r\n", "\n").replace(['\n', '\r'], " ")
}

/// [`resolve`] with an injected parent-environment map
/// (`BTreeMap<OsString, OsString>`): the `pass_env` filter consults
/// ONLY this map — never `std::env` — so resolution is deterministic
/// and directly testable (spec R3; design D3).
pub fn resolve_with_env(
    sandbox: &SandboxConfig,
    runtime: &RuntimeFacts,
    spec: &ExecutorSpec,
    parent_env: &BTreeMap<OsString, OsString>,
) -> CaduceusResult<SandboxSpec> {
    // 1. Lexically normalize every declared path so containment can
    //    be checked without filesystem access (resolve is pure).
    let undeclared = |p: &Path| CaduceusError::OciUndeclaredMount {
        path: p.display().to_string(),
    };
    let workdir_base_norm = lexical_normalize(&runtime.workdir_base).ok_or_else(|| {
        CaduceusError::OciUndeclaredMount {
            path: runtime.workdir_base.display().to_string(),
        }
    })?;
    let state_dir_norm =
        lexical_normalize(&runtime.state_dir).ok_or_else(|| undeclared(&runtime.state_dir))?;
    let worktree_norm =
        lexical_normalize(&runtime.worktree).ok_or_else(|| undeclared(&runtime.worktree))?;
    let output_norm =
        lexical_normalize(&runtime.output_dir).ok_or_else(|| undeclared(&runtime.output_dir))?;
    let shadow_norm = lexical_normalize(&runtime.git_shadow_host)
        .ok_or_else(|| undeclared(&runtime.git_shadow_host))?;

    // 2. Cross-root containment policy. Checked before the per-root
    //    allow-list so the double-RW conflict stays the reported
    //    failure when paths overlap.
    //
    //    2a. The daemon state directory must not live inside the
    //        worktree root — a misconfigured `state_dir` under
    //        `workdir_base` would put the daemon-owned `/output` and
    //        `.git` shadow surfaces inside the tree the worker can
    //        traverse via other runs' worktrees.
    if state_dir_norm.starts_with(&workdir_base_norm) {
        return Err(CaduceusError::OciMountConflict {
            detail: format!(
                "state_dir {} must not be inside workdir_base {} (the \
                 daemon-owned /output and .git shadow surfaces would live \
                 inside the worker-visible tree)",
                state_dir_norm.display(),
                workdir_base_norm.display(),
            ),
        });
    }

    //    2b. The three daemon-declared host paths must be pairwise
    //        disjoint — the old double-RW mount bug (same host path
    //        at two container paths) is rejected in every pairing.
    check_disjoint(&worktree_norm, &output_norm, "worktree", "output_dir")?;
    check_disjoint(&worktree_norm, &shadow_norm, "worktree", "git_shadow_host")?;
    check_disjoint(&output_norm, &shadow_norm, "output_dir", "git_shadow_host")?;

    // 3. Host-path allow-list: the worktree lives under
    //    `workdir_base`; the daemon-owned output dir and `.git`
    //    shadow live under the daemon state directory.
    if !worktree_norm.starts_with(&workdir_base_norm) {
        return Err(undeclared(&runtime.worktree));
    }
    if !output_norm.starts_with(&state_dir_norm) {
        return Err(undeclared(&runtime.output_dir));
    }
    if !shadow_norm.starts_with(&state_dir_norm) {
        return Err(undeclared(&runtime.git_shadow_host));
    }

    // 4. Mounts — exactly one workspace mount and one output mount,
    //    with fixed canonical container paths, plus the optional
    //    read-only `.git` shadow at `/workspace/.git`.
    let workspace_mount = MountSpec {
        host_path: worktree_norm,
        container_path: PathBuf::from(CONTAINER_WORKSPACE_PATH),
        read_only: false,
    };
    let output_mount = MountSpec {
        host_path: output_norm,
        container_path: PathBuf::from(CONTAINER_OUTPUT_PATH),
        read_only: false,
    };
    let git_shadow = select_git_shadow(runtime.git_shadow_kind, &shadow_norm);

    // 5. Identity — the closed 4-case matrix over
    //    (engine, engine_mode). `uid`/`gid` are always the probed
    //    worktree-owner facts; there is deliberately no fallback to
    //    a hard-coded identity.
    let identity = match (sandbox.engine, runtime.engine_mode) {
        (SandboxEngine::Docker, EngineMode::Rootful) => ResolvedIdentity {
            uid: runtime.worktree_uid,
            gid: runtime.worktree_gid,
            emit_user: true,
            userns: None,
        },
        // Container root maps to the unprivileged engine user via the
        // rootless user namespace; no `--user` is emitted.
        (SandboxEngine::Docker, EngineMode::Rootless) => ResolvedIdentity {
            uid: runtime.worktree_uid,
            gid: runtime.worktree_gid,
            emit_user: false,
            userns: None,
        },
        // In-container uid/gid = the user invoking Podman = the
        // daemon user = the worktree owner (the daemon created the
        // worktree). Plain `keep-id` — no uid=/gid= mapping, which is
        // what removed the hard-coded 1000 mapping.
        (SandboxEngine::Podman, EngineMode::Rootless) => ResolvedIdentity {
            uid: runtime.worktree_uid,
            gid: runtime.worktree_gid,
            emit_user: false,
            userns: Some("keep-id"),
        },
        (SandboxEngine::Podman, EngineMode::Rootful) => ResolvedIdentity {
            uid: runtime.worktree_uid,
            gid: runtime.worktree_gid,
            emit_user: true,
            userns: None,
        },
    };

    // 6. Image — reject non-digest-pinned references.
    let image = ImageRef::new(&sandbox.image)?;

    // 7. Resources — mapped 1:1 from SandboxResources.
    let resources = ResourceLimits {
        cpus: sandbox.resources.cpus,
        memory_mb: sandbox.resources.memory_mb,
        pids: sandbox.resources.pids,
        tmpfs_mb: sandbox.resources.tmpfs_mb,
        shm_mb: sandbox.resources.shm_mb,
    };

    // 9. Network — sourced from `sandbox.network` through the single
    //     `SandboxNetwork → NetworkMode` conversion. `None` renders
    //     `--network none` (loopback-only); `Unrestricted` renders the
    //     engine's default isolated bridge (NAT'd egress) — host
    //     networking is structurally unrepresentable either way.
    let network = NetworkMode::from(sandbox.network);

    // 10. Environment — exactly three sources (frozen contract,
    //     issue #249): the resolved `sandbox.pass_env` entries, the
    //     canonical `CADUCEUS_*` set, and the two compat values.
    //     Stored as a sorted map so the env-file byte layout is
    //     deterministic (design D5).
    //
    //     `pass_env` resolution (spec R4, FROZEN v1): each entry is
    //     an EXACT daemon-environment variable name. PRESENT ⇒ its
    //     value is included; ABSENT ⇒ a typed error FAILS the run
    //     BEFORE container create — never warn-and-skip. The shared
    //     `denied_name` authority is re-applied defensively here
    //     (a denied name is refused regardless of presence), and
    //     values must be valid UTF-8 free of `\n`/`\r` because the
    //     OCI env file is line-based. Error messages carry the
    //     variable NAME only — never its value.
    let mut environment: BTreeMap<String, String> = BTreeMap::new();
    for name in &sandbox.pass_env {
        let name_os = OsStr::new(name.as_str());
        if denied_name(name_os) {
            return Err(CaduceusError::Config(format!(
                "sandbox.pass_env entry {name:?} is a denied credential or \
                 daemon-internal name"
            )));
        }
        let value = match parent_env.get(name_os) {
            Some(value) => value,
            None => {
                return Err(CaduceusError::Config(format!(
                    "sandbox.pass_env name {name} not present in daemon environment"
                )));
            }
        };
        let value = value.to_str().ok_or_else(|| {
            CaduceusError::Config(format!(
                "sandbox.pass_env name {name} is not valid UTF-8 in the \
                 daemon environment"
            ))
        })?;
        if value.contains('\n') || value.contains('\r') {
            return Err(CaduceusError::Config(format!(
                "sandbox.pass_env name {name} contains a newline and cannot \
                 be transported in the OCI env file"
            )));
        }
        // Resolved entries go in first; the canonical + compat writes
        // below are authoritative even if a reserved-key collision
        // somehow bypassed config validation (design D3 step 3).
        environment.insert(name.clone(), value.to_string());
    }

    //     The canonical `CADUCEUS_*` set carries CONTAINER-side path
    //     values so host paths never leak into the container
    //     environment (issue #243). Value formatting mirrors the
    //     canonical layer of `worker_contract::sanitized_env`; only
    //     `CADUCEUS_WORKTREE_PATH` and `CADUCEUS_RESULT_PATH` are
    //     substituted for their container paths — every other
    //     canonical variable carries the same value as TrustedHost.
    //     No credential variable is ever emitted here.
    //
    //     Canonical values are newline-normalized (below) because the
    //     OCI env file is line-based and GitHub issue titles/bodies
    //     are routinely multi-line: normalization lets every real
    //     issue resolve and run instead of failing pre-create. The
    //     full multi-line content still reaches the worker verbatim
    //     through the prompt file written into the worktree
    //     (`write_prompt`). Operator `pass_env` values are NOT
    //     normalized — a newline-bearing one fails closed above
    //     (design D3).
    let labels_json = serde_json::to_string(&spec.labels)
        .map_err(|err| CaduceusError::Config(format!("labels JSON serialise: {err}")))?;
    let canonical: Vec<(String, String)> = vec![
        ("CADUCEUS_RUN_ID".to_string(), runtime.run_id.clone()),
        ("CADUCEUS_ISSUE_ID".to_string(), runtime.issue.display_key()),
        (
            "CADUCEUS_ISSUE_NUMBER".to_string(),
            spec.issue.number.to_string(),
        ),
        (
            "CADUCEUS_ISSUE_REPO".to_string(),
            format!("{}/{}", spec.issue.owner, spec.issue.repo),
        ),
        ("CADUCEUS_ISSUE_TITLE".to_string(), spec.issue_title.clone()),
        ("CADUCEUS_ISSUE_BODY".to_string(), spec.issue_body.clone()),
        (
            "CADUCEUS_ISSUE_LABELS_JSON".to_string(),
            labels_json.clone(),
        ),
        (
            "CADUCEUS_CONTEXT_JSON".to_string(),
            spec.context_json.clone(),
        ),
        ("CADUCEUS_BRANCH_NAME".to_string(), spec.branch_name.clone()),
        (
            "CADUCEUS_WORKTREE_PATH".to_string(),
            CONTAINER_WORKSPACE_PATH.to_string(),
        ),
        (
            "CADUCEUS_RESULT_PATH".to_string(),
            format!("{CONTAINER_OUTPUT_PATH}/{WORKER_RESULT_FILE}"),
        ),
    ];
    for (key, value) in canonical {
        environment.insert(key, normalize_canonical_value(&value));
    }
    // Two compat values, always present (frozen contract): the
    // container rootfs is read-only with a bounded tmpfs at `/tmp`.
    for (key, value) in [("HOME", "/tmp"), ("TMPDIR", "/tmp")] {
        environment.insert(key.to_string(), value.to_string());
    }
    let environment: Vec<(String, String)> = environment.into_iter().collect();

    // 11. Tmpfs — bounded ephemeral surfaces only: `/tmp` sized from
    //     `resources.tmpfs_mb` and `/dev/shm` sized from
    //     `resources.shm_mb` (replacing the standalone `--shm-size`
    //     flag, which sized the same mount redundantly).
    let tmpfs = vec![
        TmpfsMount {
            target: "/tmp".to_string(),
            size_mb: sandbox.resources.tmpfs_mb,
        },
        TmpfsMount {
            target: "/dev/shm".to_string(),
            size_mb: sandbox.resources.shm_mb,
        },
    ];

    // 12. Labels — fixed order.
    let labels = crate::executor::sandbox_renderer::render_labels(
        &runtime.daemon_id,
        &runtime.run_id,
        &runtime.issue.display_key(),
    );

    // 13. Command — worker argv.
    let command = runtime.worker_command.clone();

    // 14. Name — container name == run_id.
    let name = runtime.run_id.clone();

    let spec = SandboxSpec::new(
        name,
        image,
        command,
        identity,
        workspace_mount,
        output_mount,
        git_shadow,
        tmpfs,
        environment,
        resources,
        network,
        FixedSecurityPolicy,
        labels,
    );

    // 15. Writable-surface invariant — enforced where the closed spec
    //     is built, so an extra writable host-backed mount can only
    //     ever be a future regression caught at resolution time.
    validate_mount_policy(&spec)?;

    // 16. Host-escalation tripwire (defense in depth): no engine
    //     socket mount may ever appear on a resolved spec, and the
    //     network mode is re-checked against the closed two-mode
    //     allow-list — the host network namespace is never joined,
    //     structurally, on every path.
    validate_no_host_escalation(&spec)?;

    Ok(spec)
}

/// Reject two daemon-declared host paths that are equal or nested
/// (either direction). `left`/`right` are human-readable labels used
/// in the conflict detail.
fn check_disjoint(
    left: &Path,
    right: &Path,
    left_label: &str,
    right_label: &str,
) -> CaduceusResult<()> {
    if left == right || left.starts_with(right) || right.starts_with(left) {
        return Err(CaduceusError::OciMountConflict {
            detail: format!(
                "{left_label} {} and {right_label} {} must be disjoint host \
                 paths (the old double-RW mount bug: each mount needs a \
                 distinct host path)",
                left.display(),
                right.display(),
            ),
        });
    }
    Ok(())
}

/// Enforce the writable-surface invariant on a resolved spec:
///
/// - exactly two RW host-backed mounts: `/workspace` and `/output`;
/// - the only extra host-backed mount is the `.git` shadow, and it
///   is read-only at `/workspace/.git`;
/// - the tmpfs set is exactly `[/tmp (tmpfs_mb), /dev/shm (shm_mb)]`.
///
/// A violation returns `OciUndeclaredMount`/`OciMountConflict` —
/// structurally this can only fire on a future regression inside
/// `resolve` itself, which is exactly the point.
fn validate_mount_policy(spec: &SandboxSpec) -> CaduceusResult<()> {
    // The two writable host-backed surfaces.
    let writable = [
        ("workspace", &spec.workspace_mount, "/workspace"),
        ("output", &spec.output_mount, "/output"),
    ];
    for (label, mount, container_path) in writable {
        if mount.read_only {
            return Err(CaduceusError::OciMountConflict {
                detail: format!("the {label} surface must be writable, but {mount:?} is read-only"),
            });
        }
        if mount.container_path != Path::new(container_path) {
            return Err(CaduceusError::OciMountConflict {
                detail: format!(
                    "the {label} surface must target {container_path}, but {:?} was declared",
                    mount.container_path
                ),
            });
        }
    }

    // The only extra host-backed mount is the read-only `.git` shadow.
    if let Some(shadow) = &spec.git_shadow {
        if !shadow.read_only {
            return Err(CaduceusError::OciUndeclaredMount {
                path: shadow.host_path.display().to_string(),
            });
        }
        if shadow.container_path != Path::new("/workspace/.git") {
            return Err(CaduceusError::OciMountConflict {
                detail: format!(
                    "the .git shadow must target /workspace/.git, but {:?} was declared",
                    shadow.container_path
                ),
            });
        }
    }

    // Tmpfs set is exactly the two bounded ephemeral surfaces.
    let expected = [
        ("/tmp", spec.resources.tmpfs_mb),
        ("/dev/shm", spec.resources.shm_mb),
    ];
    if spec.tmpfs.len() != expected.len() {
        return Err(CaduceusError::OciMountConflict {
            detail: format!(
                "tmpfs set must be exactly {expected:?}, got {:?}",
                spec.tmpfs
            ),
        });
    }
    for (mount, (target, size_mb)) in spec.tmpfs.iter().zip(expected.iter()) {
        if mount.target != *target || mount.size_mb != *size_mb {
            return Err(CaduceusError::OciMountConflict {
                detail: format!(
                    "tmpfs set must be exactly {expected:?}, got {:?}",
                    spec.tmpfs
                ),
            });
        }
    }

    Ok(())
}

/// Reject host-escalation surfaces on a resolved spec (issue #245):
///
/// - any host-backed mount (workspace, output, `.git` shadow) whose
///   `host_path` or `container_path` file name is an engine/runtime
///   socket (`docker.sock` / `podman.sock`) — mounting the engine
///   socket is a container-escape primitive;
/// - any network mode outside the closed two-variant allow-list —
///   both [`NetworkMode::None`] (loopback-only) and
///   [`NetworkMode::Unrestricted`] (the engine's default isolated
///   bridge, NAT'd egress) preserve the
///   host-namespace-never-joined invariant; the exhaustive `match`
///   trips at compile time if a host-representing variant is ever
///   added without re-review.
///
/// `--device` and `--pid/--ipc/--uts=host` are denied *structurally*:
/// [`SandboxSpec`] has no field that could express them and the
/// renderer never emits those tokens; the golden renderer tests pin
/// their absence.
pub fn validate_no_host_escalation(spec: &SandboxSpec) -> CaduceusResult<()> {
    for (label, mount) in [
        ("workspace", spec.workspace_mount()),
        ("output", spec.output_mount()),
    ] {
        for path in [&mount.host_path, &mount.container_path] {
            if is_engine_socket_path(path) {
                return Err(CaduceusError::OciMountConflict {
                    detail: format!(
                        "the {label} mount must not target an engine/runtime \
                         socket path: {}",
                        path.display()
                    ),
                });
            }
        }
    }
    if let Some(shadow) = spec.git_shadow() {
        for path in [&shadow.host_path, &shadow.container_path] {
            if is_engine_socket_path(path) {
                return Err(CaduceusError::OciMountConflict {
                    detail: format!(
                        "the .git shadow mount must not target an \
                         engine/runtime socket path: {}",
                        path.display()
                    ),
                });
            }
        }
    }
    // Both network modes keep the host network namespace out of
    // reach: `None` is loopback-only; `Unrestricted` is the engine's
    // default isolated bridge (NAT'd outbound egress, never host).
    // The match is exhaustive over the closed enum, so a hypothetical
    // future host-representing variant fails to compile here until
    // this guard is deliberately re-reviewed (SAN-NET-4).
    match spec.network() {
        NetworkMode::None | NetworkMode::Unrestricted => {}
    }
    Ok(())
}

/// True when *path*'s file name is the Docker or Podman engine
/// socket. `pub` for testability from `tests/` per the
/// no-inline-tests rule.
pub fn is_engine_socket_path(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|s| s.to_str()),
        Some("docker.sock") | Some("podman.sock")
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
