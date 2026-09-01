//! Live OCI production-readiness checks.
//!
//! Readiness is deliberately an invocation-time observation.  The executor
//! uses the same check runner as the `doctor` command and never consults the
//! informational report written by the CLI.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use crate::executor::oci_engine::OciImageAdapter;
use crate::executor::oci_image::{self, ImageAcquisition};
use crate::executor::oci_platform::HostPlatform;
use crate::executor::sandbox_spec::SandboxEngine;
use crate::infra::config::{Config, OciPullPolicy, SandboxNetwork};
use crate::infra::disk;
use crate::infra::error::{CaduceusError, CaduceusResult, ReadinessFailure};

const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
pub const REPORT_SCHEMA_VERSION: &str = "1.0.0";

/// Stable identifiers for the mandatory readiness checks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, Serialize, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum CheckId {
    Platform,
    Engine,
    Mode,
    Namespace,
    Resources,
    Filesystem,
    Image,
    Network,
    Primitives,
}

impl CheckId {
    pub const ALL: [Self; 9] = [
        Self::Platform,
        Self::Engine,
        Self::Mode,
        Self::Namespace,
        Self::Resources,
        Self::Filesystem,
        Self::Image,
        Self::Network,
        Self::Primitives,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::Engine => "engine",
            Self::Mode => "mode",
            Self::Namespace => "namespace",
            Self::Resources => "resources",
            Self::Filesystem => "filesystem",
            Self::Image => "image",
            Self::Network => "network",
            Self::Primitives => "primitives",
        }
    }
}

impl std::fmt::Display for CheckId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Result of one readiness observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
}

/// One machine-readable readiness finding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckResult {
    pub id: CheckId,
    pub status: CheckStatus,
    pub detail: String,
    pub remediation: Option<String>,
}

impl CheckResult {
    fn pass(id: CheckId, detail: impl Into<String>) -> Self {
        Self {
            id,
            status: CheckStatus::Pass,
            detail: detail.into(),
            remediation: None,
        }
    }

    fn fail(id: CheckId, detail: impl Into<String>, remediation: impl Into<String>) -> Self {
        Self {
            id,
            status: CheckStatus::Fail,
            detail: detail.into(),
            remediation: Some(remediation.into()),
        }
    }
}

/// Overall mandatory readiness verdict.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ReadinessVerdict {
    Ready,
    Unavailable,
}

/// Optional diagnostic result.  It is intentionally not part of the verdict.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DiagnosticStatus {
    Pass,
    Skip,
    Failure,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiagnosticCanary {
    pub status: DiagnosticStatus,
    pub detail: String,
}

/// Stable report consumed by people and tooling.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReadinessReport {
    pub schema_version: String,
    pub generated_at: DateTime<Utc>,
    pub verdict: ReadinessVerdict,
    pub checks: Vec<CheckResult>,
    pub diagnostic_canary: Option<DiagnosticCanary>,
    #[serde(skip)]
    pub verified_image: Option<ImageAcquisition>,
}

impl ReadinessReport {
    pub fn mandatory_failures(&self) -> impl Iterator<Item = &CheckResult> {
        self.checks
            .iter()
            .filter(|check| check.status == CheckStatus::Fail)
    }
}

/// Pure verdict assembly.  The diagnostic canary is not an input by design.
pub fn assemble_report(checks: Vec<CheckResult>) -> ReadinessReport {
    let verdict = if checks.iter().all(|check| check.status == CheckStatus::Pass) {
        ReadinessVerdict::Ready
    } else {
        ReadinessVerdict::Unavailable
    };
    ReadinessReport {
        schema_version: REPORT_SCHEMA_VERSION.to_string(),
        generated_at: Utc::now(),
        verdict,
        checks,
        diagnostic_canary: None,
        verified_image: None,
    }
}

/// Test and production seams for host-derived paths.
#[derive(Clone, Debug)]
pub struct ProbeOptions {
    pub cgroup_root: PathBuf,
    /// Override the engine executable for deterministic tests.
    pub engine_binary: Option<PathBuf>,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            engine_binary: None,
        }
    }
}

/// Execute the mandatory checks live.  If the configuration has no sandbox,
/// the report is still returned with an actionable failure rather than a
/// panic, which makes `doctor` useful on TrustedHost-only installations.
pub async fn run_live(cfg: &Config) -> ReadinessReport {
    run_live_with_options(cfg, &ProbeOptions::default()).await
}

pub async fn run_live_with_options(cfg: &Config, options: &ProbeOptions) -> ReadinessReport {
    let Some(sandbox) = cfg.sandbox.as_ref() else {
        return assemble_report(vec![CheckResult::fail(
            CheckId::Engine,
            "no sandbox configuration is present",
            "configure executor_mode: oci and a complete sandbox section, or keep trusted_host mode",
        )]);
    };

    let mut checks = Vec::with_capacity(CheckId::ALL.len());
    let engine_path = options
        .engine_binary
        .clone()
        .map(Ok)
        .unwrap_or_else(|| which::which(sandbox.engine.binary_name()));
    checks.push(match engine_path {
        Ok(ref path) => CheckResult::pass(
            CheckId::Platform,
            if cfg!(target_os = "linux") {
                format!(
                    "Linux host; {} resolves to {}",
                    sandbox.engine.binary_name(),
                    path.display()
                )
            } else {
                format!(
                    "unsupported host platform {}; {} resolves to {}",
                    std::env::consts::OS,
                    sandbox.engine.binary_name(),
                    path.display()
                )
            },
        ),
        Err(_) => CheckResult::fail(
            CheckId::Platform,
            format!("{} is not executable on PATH", sandbox.engine.binary_name()),
            format!(
                "install {} and make it available on PATH",
                sandbox.engine.binary_name()
            ),
        ),
    });
    if !cfg!(target_os = "linux") {
        checks[0] = CheckResult::fail(
            CheckId::Platform,
            format!("unsupported host platform {}", std::env::consts::OS),
            "run OCI workers on a supported Linux host",
        );
    }

    let engine_binary = engine_path.as_ref().ok().cloned();
    let info = EngineInfo::query(sandbox.engine, engine_binary.as_deref()).await;
    match &info {
        Ok(info) => {
            checks.push(CheckResult::pass(
                CheckId::Engine,
                format!(
                    "{} daemon is reachable{}",
                    sandbox.engine.binary_name(),
                    info.version_detail()
                ),
            ));
            match info.mode(sandbox.engine) {
                Ok((mode, userns_remap)) => {
                    checks.push(CheckResult::pass(
                        CheckId::Mode,
                        format!("{} mode detected", mode_name(mode)),
                    ));
                    checks.push(if userns_remap {
                        CheckResult::fail(
                            CheckId::Namespace,
                            "rootful engine reports userns-remap",
                            "disable userns-remap or switch the engine to a supported rootless configuration",
                        )
                    } else {
                        CheckResult::pass(CheckId::Namespace, "engine namespace mapping is supported")
                    });
                }
                Err(detail) => {
                    checks.push(CheckResult::fail(
                        CheckId::Mode,
                        detail.clone(),
                        "run the configured engine's info command and repair the daemon configuration",
                    ));
                    checks.push(CheckResult::fail(
                        CheckId::Namespace,
                        "namespace safety could not be evaluated",
                        "make the engine info probe succeed before dispatching OCI workers",
                    ));
                }
            }
            checks.push(check_resources(options, info));
            checks.push(check_primitives(info));
        }
        Err(detail) => {
            checks.push(CheckResult::fail(
                CheckId::Engine,
                detail.clone(),
                format!(
                    "start the {} daemon and confirm `{}` succeeds",
                    sandbox.engine.binary_name(),
                    sandbox.engine.binary_name()
                ),
            ));
            for id in [
                CheckId::Mode,
                CheckId::Namespace,
                CheckId::Resources,
                CheckId::Primitives,
            ] {
                checks.push(CheckResult::fail(
                    id,
                    "engine-dependent check could not run",
                    "repair engine reachability and rerun `caduceus doctor`",
                ));
            }
        }
    }

    checks.push(check_filesystem(cfg));
    checks.push(check_network(sandbox.engine, sandbox.network, engine_binary.as_deref()).await);
    let (image_check, verified_image) = check_image(
        sandbox.engine,
        &sandbox.image,
        sandbox.pull_policy,
        engine_binary.as_deref(),
    )
    .await;
    checks.push(image_check);
    checks.sort_by_key(|check| check.id);
    let mut report = assemble_report(checks);
    report.verified_image = verified_image;
    report
}

/// Refuse an OCI dispatch using only current live observations.
pub async fn assert_live(cfg: &Config) -> CaduceusResult<ImageAcquisition> {
    assert_live_with_options(cfg, &ProbeOptions::default()).await
}

/// Refuse an OCI dispatch using current observations and an optional test
/// seam. The report cache is intentionally not part of this function.
pub async fn assert_live_with_options(
    cfg: &Config,
    options: &ProbeOptions,
) -> CaduceusResult<ImageAcquisition> {
    let report = run_live_with_options(cfg, options).await;
    if report.verdict == ReadinessVerdict::Ready {
        return report.verified_image.ok_or_else(|| {
            CaduceusError::Other("readiness passed without verified image facts".to_string())
        });
    }
    let failures = report
        .mandatory_failures()
        .map(|check| ReadinessFailure {
            check: check.id.to_string(),
            detail: check.detail.clone(),
            remediation: check
                .remediation
                .clone()
                .unwrap_or_else(|| "repair the failing host or configuration check".to_string()),
        })
        .collect::<Vec<_>>();
    Err(CaduceusError::OciReadinessUnavailable {
        failed_checks: failures,
    })
}

fn mode_name(mode: crate::executor::sandbox_spec::EngineMode) -> &'static str {
    match mode {
        crate::executor::sandbox_spec::EngineMode::Rootful => "rootful",
        crate::executor::sandbox_spec::EngineMode::Rootless => "rootless",
    }
}

#[derive(Clone, Debug)]
struct EngineInfo {
    mode_output: String,
    json: serde_json::Value,
}

impl EngineInfo {
    async fn query(engine: SandboxEngine, binary: Option<&Path>) -> Result<Self, String> {
        let mode_format = match engine {
            SandboxEngine::Docker => "{{.SecurityOptions}}",
            SandboxEngine::Podman => "{{.Host.Security.Rootless}}",
        };
        let json_format = match engine {
            SandboxEngine::Docker => "{{json .}}",
            SandboxEngine::Podman => "json",
        };
        let mode = run_engine(engine, binary, ["info", "--format", mode_format]).await?;
        let json = run_engine(engine, binary, ["info", "--format", json_format]).await?;
        let parsed = serde_json::from_str(&json)
            .map_err(|err| format!("engine info JSON is invalid: {err}"))?;
        Ok(Self {
            mode_output: mode,
            json: parsed,
        })
    }

    fn mode(
        &self,
        engine: SandboxEngine,
    ) -> Result<(crate::executor::sandbox_spec::EngineMode, bool), String> {
        crate::executor::engine_probe::parse_engine_mode(engine, &self.mode_output)
            .map_err(|err| err.to_string())
    }

    fn version_detail(&self) -> String {
        find_string(&self.json, &["ServerVersion", "Version"])
            .map(|version| format!(" (server {version})"))
            .unwrap_or_default()
    }
}

async fn run_engine<I, S>(
    engine: SandboxEngine,
    binary: Option<&Path>,
    args: I,
) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = tokio::time::timeout(
        CHECK_TIMEOUT,
        tokio::process::Command::new(binary.unwrap_or_else(|| Path::new(engine.binary_name())))
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| "engine command timed out".to_string())?
    .map_err(|err| format!("engine command could not start: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("engine command exited with {}", output.status)
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn check_resources(options: &ProbeOptions, info: &EngineInfo) -> CheckResult {
    let version = find_string(&info.json, &["CgroupVersion", "CgroupVersion"]);
    let required = ["cpu", "memory", "pids"];
    let controllers_path = options.cgroup_root.join("cgroup.controllers");
    if let Some(version) = version {
        if version == "2" {
            match std::fs::read_to_string(&controllers_path) {
                Ok(contents) => {
                    let found: BTreeSet<&str> = contents.split_whitespace().collect();
                    let missing: Vec<&str> = required
                        .iter()
                        .copied()
                        .filter(|name| !found.contains(name))
                        .collect();
                    if missing.is_empty() {
                        return CheckResult::pass(
                            CheckId::Resources,
                            "cgroup v2 exposes cpu, memory, and pids",
                        );
                    }
                    return CheckResult::fail(
                        CheckId::Resources,
                        format!(
                            "cgroup v2 is missing controller(s): {}",
                            missing.to_vec().join(", ")
                        ),
                        "delegate cpu, memory, and pids controllers to the engine's cgroup scope",
                    );
                }
                Err(err) => {
                    return CheckResult::fail(
                        CheckId::Resources,
                        format!("cannot read {}: {err}", controllers_path.display()),
                        "make the engine cgroup scope readable and delegate cpu, memory, and pids",
                    )
                }
            }
        }
    }
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|name| !options.cgroup_root.join(name).exists())
        .collect();
    if missing.is_empty() {
        CheckResult::pass(CheckId::Resources, "cgroup controller paths are available")
    } else {
        CheckResult::fail(
            CheckId::Resources,
            format!(
                "required cgroup controller(s) are unavailable: {}",
                missing.to_vec().join(", ")
            ),
            "mount and delegate cpu, memory, and pids cgroup controllers to the engine",
        )
    }
}

fn check_primitives(info: &EngineInfo) -> CheckResult {
    let driver = find_string(&info.json, &["CgroupDriver", "CgroupManager", "Driver"]);
    match driver {
        Some(driver) if !driver.eq_ignore_ascii_case("none") => CheckResult::pass(
            CheckId::Primitives,
            format!("engine reports cgroup driver {driver}"),
        ),
        Some(_) => CheckResult::fail(
            CheckId::Primitives,
            "engine reports no usable cgroup driver",
            "configure the engine with cgroup resource-control support",
        ),
        None => CheckResult::fail(
            CheckId::Primitives,
            "engine did not report a cgroup driver",
            "upgrade or configure the engine so its resource driver is exposed",
        ),
    }
}

fn check_filesystem(cfg: &Config) -> CheckResult {
    let paths = [
        ("state", cfg.state_dir.clone(), false),
        ("repository-storage", cfg.repo_storage_root.clone(), true),
        ("worktree", cfg.workdir_base.clone(), false),
    ];
    let watchdog_paths: Vec<PathBuf> = paths.iter().map(|(_, path, _)| path.clone()).collect();
    for (role, path, strict_mode) in &paths {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(err) => {
                return CheckResult::fail(
                    CheckId::Filesystem,
                    format!("required {role} directory is missing: {} ({err})", path.display()),
                    "create the state, repository-storage, and worktree directories with daemon ownership",
                )
            }
        };
        if metadata.file_type().is_symlink() {
            return CheckResult::fail(
                CheckId::Filesystem,
                format!("required directory is a symlink: {}", path.display()),
                "replace the symlink with a daemon-owned directory",
            );
        }
        if !metadata.is_dir() {
            return CheckResult::fail(
                CheckId::Filesystem,
                format!("required path is not a directory: {}", path.display()),
                "replace the configured path with a daemon-owned directory",
            );
        }
        #[cfg(unix)]
        {
            let mode = metadata.permissions().mode() & 0o777;
            let mode_ok = if *strict_mode {
                mode == 0o700
            } else {
                mode & 0o022 == 0
            };
            if !mode_ok {
                return CheckResult::fail(
                    CheckId::Filesystem,
                    if *strict_mode {
                        format!("{} has mode {mode:03o}; expected 700", path.display())
                    } else {
                        format!(
                            "{} has mode {mode:03o}; group/other write access is not allowed",
                            path.display()
                        )
                    },
                    if *strict_mode {
                        format!("chmod 700 {}", path.display())
                    } else {
                        format!("chmod u+rwX,go-w {}", path.display())
                    },
                );
            }
            let owner = nix::unistd::Uid::effective().as_raw();
            if metadata.uid() != owner {
                return CheckResult::fail(
                    CheckId::Filesystem,
                    format!(
                        "{} is owned by uid {}, expected daemon uid {}",
                        path.display(),
                        metadata.uid(),
                        owner
                    ),
                    format!("chown {} {}", owner, path.display()),
                );
            }
        }
    }
    let samples = match disk::sample_free_bytes(&watchdog_paths) {
        Ok(samples) => samples,
        Err(err) => {
            return CheckResult::fail(
                CheckId::Filesystem,
                format!("filesystem probe failed: {err}"),
                "make all configured storage paths readable and writable, then rerun doctor",
            )
        }
    };
    let reserve = cfg
        .sandbox()
        .reserved_host_disk_mb
        .saturating_mul(1024 * 1024);
    if let Some(sample) = samples.iter().find(|sample| sample.free_bytes < reserve) {
        return CheckResult::fail(
            CheckId::Filesystem,
            format!(
                "{} has {} bytes free below the {} byte reserve",
                sample.representative_path.display(),
                sample.free_bytes,
                reserve
            ),
            "free space on every filesystem hosting Caduceus state and worktrees",
        );
    }
    CheckResult::pass(
        CheckId::Filesystem,
        "configured storage paths and disk reserve are healthy",
    )
}

async fn check_network(
    engine: SandboxEngine,
    network: SandboxNetwork,
    binary: Option<&Path>,
) -> CheckResult {
    if network == SandboxNetwork::None {
        return CheckResult::pass(
            CheckId::Network,
            "network none is representable by the configured engine",
        );
    }
    match run_engine(engine, binary, ["network", "inspect", "bridge"]).await {
        Ok(_) => CheckResult::pass(CheckId::Network, "the default bridge network is available"),
        Err(detail) => CheckResult::fail(
            CheckId::Network,
            format!("default bridge network is unavailable: {detail}"),
            "create or repair the engine's default bridge network, or select network: none",
        ),
    }
}

async fn check_image(
    engine: SandboxEngine,
    image_ref: &str,
    policy: OciPullPolicy,
    binary: Option<&Path>,
) -> (CheckResult, Option<ImageAcquisition>) {
    if !is_digest_pinned(image_ref) {
        return (
            CheckResult::fail(
                CheckId::Image,
                "configured image is not digest-pinned",
                "set sandbox.image to name@sha256:<64 hex characters>",
            ),
            None,
        );
    }
    let adapter = binary
        .map(|path| OciImageAdapter::with_binary(engine, path))
        .unwrap_or_else(|| OciImageAdapter::new(engine));
    let host = crate::executor::oci_platform::host_platform();
    let acquisition =
        match oci_image::acquire_image_with_adapter(&adapter, image_ref, policy, &host).await {
            Ok(acquisition) => acquisition,
            Err(err) => return (image_failure(err), None),
        };
    (
        CheckResult::pass(
            CheckId::Image,
            format!(
                "{} is present, digest-verified, and matches {}",
                image_ref,
                platform_name(&host)
            ),
        ),
        Some(acquisition),
    )
}

fn image_failure(err: CaduceusError) -> CheckResult {
    CheckResult::fail(
        CheckId::Image,
        err.to_string(),
        "make the configured immutable image present or pullable for this host architecture",
    )
}

fn platform_name(platform: &HostPlatform) -> String {
    platform
        .variant
        .as_deref()
        .map(|variant| format!("{}/{}", platform.architecture, variant))
        .unwrap_or_else(|| platform.architecture.clone())
}

fn is_digest_pinned(reference: &str) -> bool {
    let Some((name, digest)) = reference.rsplit_once("@sha256:") else {
        return false;
    };
    !name.is_empty() && digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn find_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for key in keys {
                if let Some(value) = map.get(*key).and_then(serde_json::Value::as_str) {
                    return Some(value.to_string());
                }
            }
            map.values().find_map(|value| find_string(value, keys))
        }
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| find_string(value, keys))
        }
        _ => None,
    }
}

/// Persist a report for display only.  The executor never calls this helper.
pub fn write_informational_report(
    state_dir: &Path,
    report: &ReadinessReport,
) -> CaduceusResult<()> {
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join("doctor.json");
    let bytes = serde_json::to_vec_pretty(report)?;
    let temporary = state_dir.join("doctor.json.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::set_permissions(
        &temporary,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

/// Render the report for operators without conflating diagnostic output with
/// the mandatory verdict.
pub fn render_human(report: &ReadinessReport) -> String {
    let mut output = String::from("caduceus doctor\n");
    for check in &report.checks {
        let label = match check.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Fail => "FAIL",
        };
        output.push_str(&format!("  [{label}] {}: {}\n", check.id, check.detail));
        if let Some(remediation) = &check.remediation {
            output.push_str(&format!("         remediation: {remediation}\n"));
        }
    }
    output.push_str(&format!(
        "  readiness: {}\n",
        match report.verdict {
            ReadinessVerdict::Ready => "READY",
            ReadinessVerdict::Unavailable => "UNAVAILABLE",
        }
    ));
    if let Some(canary) = &report.diagnostic_canary {
        output.push_str(&format!(
            "  diagnostic canary: {}: {}\n",
            match canary.status {
                DiagnosticStatus::Pass => "PASS",
                DiagnosticStatus::Skip => "SKIP",
                DiagnosticStatus::Failure => "FAILURE",
            },
            canary.detail,
        ));
    }
    output
}
