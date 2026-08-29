//! Opt-in live OCI image tests.
//!
//! These tests are intentionally inert unless `CADUCEUS_OCI_LIVE` selects an
//! engine. A selected suite also requires `CADUCEUS_LIVE_TEST_IMAGE`, a
//! digest-pinned reference suitable for the local registry credentials.
//!
//! Docker/Podman image command differences remain confined to
//! `OciImageAdapter`; these live cases share the same acquisition path.

use std::path::PathBuf;
use std::process::Command;

use caduceus::executor::oci_engine::{NormalizedImage, OciImageAdapter};
use caduceus::executor::oci_image::{ensure_image_with_adapter, verify_digest};
use caduceus::executor::oci_platform::{host_platform, HostPlatform};
use caduceus::executor::SandboxEngine;
use caduceus::infra::config::OciPullPolicy;
use caduceus::infra::error::CaduceusError;
use serde_json::Value;
use tempfile::TempDir;

fn selected(engine: SandboxEngine) -> bool {
    match std::env::var("CADUCEUS_OCI_LIVE").ok().as_deref() {
        None | Some("") => {
            println!("skipped: set CADUCEUS_OCI_LIVE=docker|podman|1");
            false
        }
        Some("1") => true,
        Some(value) => value == engine.binary_name(),
    }
}

fn configured_image() -> Option<String> {
    match std::env::var("CADUCEUS_LIVE_TEST_IMAGE") {
        Ok(reference) if reference.contains("@sha256:") => Some(reference),
        Ok(_) => panic!("CADUCEUS_LIVE_TEST_IMAGE must be digest-pinned"),
        Err(_) => {
            println!("skipped: set CADUCEUS_LIVE_TEST_IMAGE to a digest-pinned image");
            None
        }
    }
}

fn engine_available(engine: SandboxEngine) -> bool {
    let format = match engine {
        SandboxEngine::Docker => "{{.SecurityOptions}}",
        SandboxEngine::Podman => "{{.Host.Security.Rootless}}",
    };
    match Command::new(engine.binary_name())
        .args(["info", "--format", format])
        .output()
    {
        Ok(output) if output.status.success() => true,
        _ => {
            println!("skipped: {} daemon is unavailable", engine.binary_name());
            false
        }
    }
}

struct LiveRun {
    _temp: TempDir,
    run_dir: PathBuf,
    image: NormalizedImage,
    image_ref: String,
    engine: SandboxEngine,
}

async fn acquire(engine: SandboxEngine, label: &str) -> Option<LiveRun> {
    if !selected(engine) || !engine_available(engine) {
        return None;
    }
    let image_ref = configured_image()?;
    let temp = tempfile::tempdir().expect("live test tempdir");
    let run_dir = temp.path().join(label);
    let result = ensure_image_with_adapter(
        OciImageAdapter::new(engine),
        engine,
        &image_ref,
        OciPullPolicy::IfMissing,
        &host_platform(),
        &run_dir,
        &format!("live-{label}"),
    )
    .await;
    let image = match result {
        Ok(image) => image,
        Err(CaduceusError::OciPullFailed { stderr, .. }) => {
            println!(
                "skipped: {} image pull unavailable: {stderr}",
                engine.binary_name()
            );
            return None;
        }
        Err(error) => panic!("live image acquisition failed: {error}"),
    };
    Some(LiveRun {
        _temp: temp,
        run_dir,
        image,
        image_ref,
        engine,
    })
}

fn assert_no_container(engine: SandboxEngine, run_id: &str) {
    let filter = format!("name={run_id}");
    let output = Command::new(engine.binary_name())
        .args(["ps", "-a", "--filter", &filter, "--format", "{{.ID}}"])
        .output()
        .expect("query live containers");
    assert!(output.status.success(), "container query failed");
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "verification failure must not create a container"
    );
}

async fn run_tier(engine: SandboxEngine, label: &str) {
    let Some(run) = acquire(engine, label).await else {
        return;
    };
    assert!(!run.image.id.is_empty());
    assert_no_container(run.engine, &format!("live-{label}"));
}

#[tokio::test]
async fn docker_tier_pulls_a_real_digest_pinned_image() {
    run_tier(SandboxEngine::Docker, "docker-pull").await;
}

#[tokio::test]
async fn podman_tier_pulls_a_real_digest_pinned_image() {
    run_tier(SandboxEngine::Podman, "podman-pull").await;
}

async fn wrong_digest_tier(engine: SandboxEngine, label: &str) {
    let Some(run) = acquire(engine, label).await else {
        return;
    };
    let base = run
        .image_ref
        .split_once('@')
        .map(|(base, _)| base)
        .expect("digest-pinned reference");
    let wrong_ref = format!("{base}@sha256:{}", "b".repeat(64));
    assert!(matches!(
        verify_digest(&run.image, &wrong_ref),
        Err(CaduceusError::OciImageDigestMismatch { .. })
    ));
    assert_no_container(run.engine, &format!("live-{label}"));
}

#[tokio::test]
async fn docker_tier_rejects_a_wrong_digest_before_create() {
    wrong_digest_tier(SandboxEngine::Docker, "docker-digest").await;
}

#[tokio::test]
async fn podman_tier_rejects_a_wrong_digest_before_create() {
    wrong_digest_tier(SandboxEngine::Podman, "podman-digest").await;
}

async fn arch_mismatch_tier(engine: SandboxEngine, label: &str) {
    let Some(run) = acquire(engine, label).await else {
        return;
    };
    let bad_host = HostPlatform {
        architecture: "unsupported-live-architecture".to_string(),
        variant: None,
    };
    let error = ensure_image_with_adapter(
        OciImageAdapter::new(engine),
        engine,
        &run.image_ref,
        OciPullPolicy::IfMissing,
        &bad_host,
        &run.run_dir,
        &format!("live-{label}-arch"),
    )
    .await
    .expect_err("wrong host architecture must abort");
    assert!(matches!(
        error,
        CaduceusError::OciImageArchitectureMismatch { .. }
    ));
    assert_no_container(run.engine, &format!("live-{label}"));
}

#[tokio::test]
async fn docker_tier_rejects_an_architecture_mismatch_before_create() {
    arch_mismatch_tier(SandboxEngine::Docker, "docker-arch").await;
}

#[tokio::test]
async fn podman_tier_rejects_an_architecture_mismatch_before_create() {
    arch_mismatch_tier(SandboxEngine::Podman, "podman-arch").await;
}

async fn provenance_tier(engine: SandboxEngine, label: &str) {
    let Some(run) = acquire(engine, label).await else {
        return;
    };
    let line =
        std::fs::read_to_string(run.run_dir.join("provenance.log")).expect("live provenance log");
    let record: Value = serde_json::from_str(line.lines().next().expect("provenance line"))
        .expect("provenance JSON");
    for field in [
        "reference",
        "resolved_id",
        "repo_digests",
        "policy",
        "engine",
        "mode",
        "timings",
    ] {
        assert!(
            !record[field].is_null(),
            "required provenance field {field}"
        );
    }
}

#[tokio::test]
async fn docker_tier_records_provenance() {
    provenance_tier(SandboxEngine::Docker, "docker-provenance").await;
}

#[tokio::test]
async fn podman_tier_records_provenance() {
    provenance_tier(SandboxEngine::Podman, "podman-provenance").await;
}
