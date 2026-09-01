//! Pull-policy orchestration, image verification, and provenance auditing.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use crate::executor::oci_engine::{NormalizedImage, OciImageAdapter};
use crate::executor::oci_platform::HostPlatform;
use crate::executor::sandbox_spec::SandboxEngine;
use crate::infra::config::OciPullPolicy;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Action selected after probing local image presence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullAction {
    Pull,
    ProbeLocalOnly,
    UseLocal,
}

/// Decide the image action without contacting a registry.
pub fn decide_pull_action(policy: OciPullPolicy, local_present: bool) -> PullAction {
    match policy {
        OciPullPolicy::Never if local_present => PullAction::UseLocal,
        OciPullPolicy::Never => PullAction::ProbeLocalOnly,
        OciPullPolicy::IfMissing if local_present => PullAction::UseLocal,
        OciPullPolicy::IfMissing => PullAction::Pull,
        OciPullPolicy::Always => PullAction::Pull,
    }
}

/// Verify that the configured digest is represented by a repository digest.
pub fn verify_digest(image: &NormalizedImage, expected_ref: &str) -> CaduceusResult<()> {
    let expected = expected_ref
        .rsplit_once("@sha256:")
        .map(|(_, digest)| format!("sha256:{digest}"))
        .unwrap_or_else(|| "sha256:<invalid>".to_string())
        .to_ascii_lowercase();
    let expected_suffix = format!("@{expected}");
    if image
        .repo_digests
        .iter()
        .any(|entry| entry.to_ascii_lowercase().ends_with(&expected_suffix))
    {
        return Ok(());
    }
    Err(CaduceusError::OciImageDigestMismatch {
        reference: expected_ref.to_string(),
        expected,
        found: image.repo_digests.clone(),
    })
}

/// Verify a normalized image against a host platform.
///
/// The two-argument form is a public test seam. The production orchestrator
/// uses the reference-aware helper below so operator errors identify the
/// configured reference exactly.
pub fn verify_arch(image: &NormalizedImage, host: &HostPlatform) -> CaduceusResult<()> {
    verify_arch_for_reference(image, host, &image.id)
}

fn verify_arch_for_reference(
    image: &NormalizedImage,
    host: &HostPlatform,
    reference: &str,
) -> CaduceusResult<()> {
    let host_arch = host.architecture.to_ascii_lowercase();
    let image_arch = image.architecture.to_ascii_lowercase();
    let known_host_arch = [
        "amd64", "arm64", "arm", "ppc64le", "s390x", "riscv64", "386",
    ]
    .contains(&host_arch.as_str());
    let variant_matches = match host.variant.as_deref() {
        None => true,
        Some(expected) => image
            .variant
            .as_deref()
            .map(|found| found.eq_ignore_ascii_case(expected))
            .unwrap_or(false),
    };
    if !known_host_arch || image_arch != host_arch || !variant_matches {
        return Err(CaduceusError::OciImageArchitectureMismatch {
            reference: reference.to_string(),
            expected: platform_label(host),
            found: platform_label(&HostPlatform {
                architecture: image.architecture.clone(),
                variant: image.variant.clone(),
            }),
        });
    }
    Ok(())
}

/// A single per-run provenance record.
#[derive(Clone, Debug, Serialize)]
pub struct ProvenanceRecord {
    pub run_id: String,
    pub engine: String,
    pub reference: String,
    pub resolved_id: Option<String>,
    pub repo_digests: Option<Vec<String>>,
    pub architecture: Option<String>,
    pub variant: Option<String>,
    pub host_arch: String,
    pub host_variant: Option<String>,
    pub policy: String,
    pub pull_attempted: bool,
    pub cache_hit: bool,
    pub mode: String,
    pub stage: String,
    pub outcome: String,
    pub error: Option<ProvenanceError>,
    pub verification: Option<VerificationRecord>,
    pub timings: ProvenanceTimings,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProvenanceError {
    pub variant: String,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VerificationRecord {
    pub status: String,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ProvenanceTimings {
    pub probe_ms: u64,
    pub pull_ms: u64,
    pub inspect_ms: u64,
    pub verify_ms: u64,
    pub total_ms: u64,
}

/// Facts collected while acquiring and verifying an image. Readiness passes
/// these facts to dispatch so the image is not acquired twice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageAcquisition {
    pub image: NormalizedImage,
    pub pull_attempted: bool,
    pub cache_hit: bool,
    pub verification: Option<VerificationRecord>,
    pub timings: ProvenanceTimings,
}

struct ImageAttempt {
    result: CaduceusResult<ImageAcquisition>,
    local_present: bool,
    pull_attempted: bool,
    stage: String,
    verification: Option<VerificationRecord>,
    timings: ProvenanceTimings,
}

/// Ensure the image is pulled, inspected, verified, and audited.
pub async fn ensure_image(
    engine: SandboxEngine,
    image_ref: &str,
    policy: OciPullPolicy,
    host: &HostPlatform,
    run_dir: &Path,
    run_id: &str,
) -> CaduceusResult<NormalizedImage> {
    ensure_image_with_adapter(
        OciImageAdapter::new(engine),
        engine,
        image_ref,
        policy,
        host,
        run_dir,
        run_id,
    )
    .await
}

/// Test seam for driving the complete orchestrator with an offline fake CLI.
#[doc(hidden)]
pub async fn ensure_image_with_adapter(
    adapter: OciImageAdapter,
    engine: SandboxEngine,
    image_ref: &str,
    policy: OciPullPolicy,
    host: &HostPlatform,
    run_dir: &Path,
    run_id: &str,
) -> CaduceusResult<NormalizedImage> {
    let ImageAttempt {
        result,
        local_present,
        pull_attempted,
        stage,
        verification,
        timings,
    } = acquire_image_attempt(&adapter, image_ref, policy, host).await;
    let success = result.is_ok();
    let resolved = result.as_ref().ok().map(|acquisition| &acquisition.image);
    let record = ProvenanceRecord {
        run_id: run_id.to_string(),
        engine: engine.binary_name().to_string(),
        reference: image_ref.to_string(),
        resolved_id: resolved.map(|image| image.id.clone()),
        repo_digests: resolved.map(|image| image.repo_digests.clone()),
        architecture: resolved.map(|image| image.architecture.clone()),
        variant: resolved.and_then(|image| image.variant.clone()),
        host_arch: host.architecture.clone(),
        host_variant: host.variant.clone(),
        policy: policy_name(policy).to_string(),
        pull_attempted,
        cache_hit: local_present && policy != OciPullPolicy::Always,
        mode: if pull_attempted { "pulled" } else { "local" }.to_string(),
        stage: if success {
            "acquired".to_string()
        } else {
            stage
        },
        outcome: if success { "ok" } else { "aborted" }.to_string(),
        error: result.as_ref().err().map(|error| ProvenanceError {
            variant: error_variant(error).to_string(),
            detail: error.to_string(),
        }),
        verification,
        timings,
    };
    write_provenance(run_dir, &record);
    result.map(|acquisition| acquisition.image)
}

/// Acquire and verify an image without writing provenance. The readiness
/// runner uses this once, then the executor records the returned facts after
/// the per-run directory and run ID exist.
pub async fn acquire_image_with_adapter(
    adapter: &OciImageAdapter,
    image_ref: &str,
    policy: OciPullPolicy,
    host: &HostPlatform,
) -> CaduceusResult<ImageAcquisition> {
    acquire_image_attempt(adapter, image_ref, policy, host)
        .await
        .result
}

async fn acquire_image_attempt(
    adapter: &OciImageAdapter,
    image_ref: &str,
    policy: OciPullPolicy,
    host: &HostPlatform,
) -> ImageAttempt {
    let started = Instant::now();
    let mut stage = "presence_probe".to_string();
    let mut local_present = false;
    let mut pull_attempted = false;
    let mut verification = None;
    let mut timings = ProvenanceTimings::default();
    let result = async {
        let probe_started = Instant::now();
        let presence = adapter.image_exists(image_ref).await;
        timings.probe_ms = elapsed_ms(probe_started);
        local_present = presence?;

        match decide_pull_action(policy, local_present) {
            PullAction::ProbeLocalOnly => {
                return Err(CaduceusError::OciImageMissing {
                    reference: image_ref.to_string(),
                });
            }
            PullAction::UseLocal => {}
            PullAction::Pull => {
                stage = "pull".to_string();
                pull_attempted = true;
                let pull_started = Instant::now();
                let pull = adapter.pull(image_ref).await;
                timings.pull_ms = elapsed_ms(pull_started);
                pull?;
            }
        }

        stage = "inspect".to_string();
        let inspect_started = Instant::now();
        let inspected = adapter.inspect(image_ref).await;
        timings.inspect_ms = elapsed_ms(inspect_started);
        let image = inspected?;

        stage = "digest_verify".to_string();
        let verify_started = Instant::now();
        if let Err(error) = verify_digest(&image, image_ref) {
            verification = Some(VerificationRecord {
                status: "failed".to_string(),
                detail: "digest mismatch".to_string(),
            });
            timings.verify_ms = elapsed_ms(verify_started);
            return Err(error);
        }

        stage = "arch_verify".to_string();
        if let Err(error) = verify_arch_for_reference(&image, host, image_ref) {
            verification = Some(VerificationRecord {
                status: "failed".to_string(),
                detail: "architecture mismatch".to_string(),
            });
            timings.verify_ms = elapsed_ms(verify_started);
            return Err(error);
        }
        timings.verify_ms = elapsed_ms(verify_started);
        verification = Some(VerificationRecord {
            status: "passed".to_string(),
            detail: "digest+arch ok".to_string(),
        });
        Ok(ImageAcquisition {
            image,
            pull_attempted,
            cache_hit: local_present && policy != OciPullPolicy::Always,
            verification: verification.clone(),
            timings: timings.clone(),
        })
    }
    .await;

    timings.total_ms = elapsed_ms(started);
    let result = result.map(|mut acquisition| {
        acquisition.timings = timings.clone();
        acquisition
    });
    ImageAttempt {
        result,
        local_present,
        pull_attempted,
        stage,
        verification,
        timings,
    }
}

/// Record a successful acquisition performed by the live readiness gate.
pub fn write_acquisition_provenance(
    run_dir: &Path,
    engine: SandboxEngine,
    image_ref: &str,
    policy: OciPullPolicy,
    host: &HostPlatform,
    run_id: &str,
    acquisition: &ImageAcquisition,
) {
    let image = &acquisition.image;
    write_provenance(
        run_dir,
        &ProvenanceRecord {
            run_id: run_id.to_string(),
            engine: engine.binary_name().to_string(),
            reference: image_ref.to_string(),
            resolved_id: Some(image.id.clone()),
            repo_digests: Some(image.repo_digests.clone()),
            architecture: Some(image.architecture.clone()),
            variant: image.variant.clone(),
            host_arch: host.architecture.clone(),
            host_variant: host.variant.clone(),
            policy: policy_name(policy).to_string(),
            pull_attempted: acquisition.pull_attempted,
            cache_hit: acquisition.cache_hit,
            mode: if acquisition.pull_attempted {
                "pulled".to_string()
            } else {
                "local".to_string()
            },
            stage: "acquired".to_string(),
            outcome: "ok".to_string(),
            error: None,
            verification: acquisition.verification.clone(),
            timings: acquisition.timings.clone(),
        },
    );
}

fn write_provenance(run_dir: &Path, record: &ProvenanceRecord) {
    if let Err(error) = fs::create_dir_all(run_dir) {
        tracing::warn!(path = %run_dir.display(), error = %error, "cannot create OCI provenance directory");
    } else {
        let path = run_dir.join("provenance.log");
        match serde_json::to_string(record) {
            Ok(mut line) => {
                line.push('\n');
                match OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)
                {
                    Ok(mut file) => {
                        if let Err(error) = file
                            .set_permissions(std::fs::Permissions::from_mode(0o600))
                            .and_then(|_| file.write_all(line.as_bytes()))
                        {
                            tracing::warn!(path = %path.display(), error = %error, "cannot write OCI provenance record");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(path = %path.display(), error = %error, "cannot open OCI provenance log");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "cannot serialize OCI provenance record");
            }
        }
    }

    tracing::info!(
        target: "caduceus_oci_provenance",
        run_id = %record.run_id,
        engine = %record.engine,
        reference = %record.reference,
        resolved_id = ?record.resolved_id,
        repo_digests = ?record.repo_digests,
        architecture = ?record.architecture,
        variant = ?record.variant,
        host_arch = %record.host_arch,
        host_variant = ?record.host_variant,
        policy = %record.policy,
        pull_attempted = record.pull_attempted,
        cache_hit = record.cache_hit,
        mode = %record.mode,
        stage = %record.stage,
        outcome = %record.outcome,
        error = ?record.error,
        verification = ?record.verification,
        timings = ?record.timings,
        "OCI image provenance"
    );
}

fn platform_label(platform: &HostPlatform) -> String {
    match &platform.variant {
        Some(variant) => format!("{}/{}", platform.architecture, variant),
        None => platform.architecture.clone(),
    }
}

fn policy_name(policy: OciPullPolicy) -> &'static str {
    match policy {
        OciPullPolicy::Never => "never",
        OciPullPolicy::IfMissing => "if_missing",
        OciPullPolicy::Always => "always",
    }
}

fn error_variant(error: &CaduceusError) -> &'static str {
    match error {
        CaduceusError::OciImageDigestMismatch { .. } => "OciImageDigestMismatch",
        CaduceusError::OciImageArchitectureMismatch { .. } => "OciImageArchitectureMismatch",
        CaduceusError::OciImageMissing { .. } => "OciImageMissing",
        CaduceusError::OciImageInspectFailed { .. } => "OciImageInspectFailed",
        CaduceusError::OciPullFailed { .. } => "OciPullFailed",
        _ => "CaduceusError",
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}
